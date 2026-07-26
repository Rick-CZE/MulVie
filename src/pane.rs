//! A single compartment: an assigned folder, the current photo, and all the
//! per-pane interaction (click-to-navigate, drag-to-pan when zoomed, the
//! right-click menu, and animated-GIF playback).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, Context, FontId, Id, Painter, Pos2, Rect, Sense, Ui, Vec2,
};

use crate::gallery::{self, SortOrder};
use crate::image_store::{ImageStore, LoadedImage};
use crate::theme;

/// Fraction of pane width on each side that navigates prev/next on click.
const SIDE: f32 = 0.18;

/// A file dialog the pane wants opened (the app runs it off-thread so video
/// keeps playing while the dialog is up). Opening a file loads its whole
/// parent folder as the sibling list, positioned at that file — so there is no
/// separate "open folder" mode any more.
#[derive(Clone, Copy)]
pub enum DialogMode {
    File,
}

/// What happened on the most recent `next`/`prev` at a folder boundary, so the
/// app can play the matching transient overlay. `None` on an ordinary step.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum NavFlash {
    /// Loop is off and we were already at the end/start — nothing moved.
    Blocked,
    /// Loop is on and we wrapped last→first (or first→last).
    Wrapped,
}

impl NavFlash {
    /// How long the overlay animation runs, in seconds.
    pub fn duration(self) -> f64 {
        match self {
            NavFlash::Blocked => 0.5,
            NavFlash::Wrapped => 0.38,
        }
    }
}

pub struct Pane {
    pub index: usize, // 0..4, matches Quadrant
    pub folder: Option<PathBuf>,
    pub sort: SortOrder,
    /// The COMPLETE scanned tree (before the type/search filter below).
    all_files: Vec<PathBuf>,
    /// Extensions currently included by Gallery management's file-type filter
    /// (defaults to everything supported).
    pub type_filter: std::collections::HashSet<String>,
    /// Case-insensitive substring the file NAME must contain ("" = no filter).
    pub search: String,
    /// What the pane actually lists/plays: `all_files` after the filters.
    pub files: Vec<PathBuf>,
    pub cursor: usize, // index into `files`
    pub zoom: f32,
    pub pan: Vec2,
    /// Set when the user asks to assign a file; consumed by the app.
    pub dialog_request: Option<DialogMode>,
    /// "Delete file" was clicked once — the menu now shows the confirm step.
    /// Reset by the app whenever no context menu is open.
    pub delete_armed: bool,
    /// The file captured when delete was armed — deleting this exact path (not
    /// whatever the pane shows at confirm time) is what makes the two-step
    /// delete safe against navigation while the menu is open.
    pub delete_target: Option<PathBuf>,
    /// Confirmed delete request carrying the captured target; consumed by the
    /// app (which owns the video/pdf players that may hold the file open).
    pub delete_request: Option<PathBuf>,
    /// Swap this pane's whole content with the given pane; consumed by the app.
    pub switch_request: Option<usize>,
    /// Mirror of the app-wide loop toggle, synced every frame.
    pub loop_folder: bool,
    /// Boundary event from the last `next`/`prev`; the app takes and animates it.
    pub last_nav: Option<NavFlash>,
    /// Clockwise quarter-turns (0..4) applied to the current item's view.
    pub rotation: u8,
    /// Files un-ticked in List Management: still on disk and still listed, but
    /// navigation skips them. Keyed by path so the set survives re-sorting;
    /// reset whenever a new folder loads. The CURRENT file may be in here —
    /// it stays on screen until the user navigates away (never a blank flash).
    pub excluded: HashSet<PathBuf>,
    /// "List management" was clicked in the right-click menu; the app consumes
    /// this and opens the List-Management window targeting this pane.
    pub list_manage_request: bool,
    /// Mirror of "which panels host a pinned List Management" (synced every
    /// frame like `loop_folder`), so the switch submenu can disable them.
    pub lm_occupied: [bool; 4],
    /// Locked: this pane ignores GLOBAL commands (resume/pause/stop-all, the
    /// arrow-key all-panes nav, clear-all) — frost still covers it. Per-pane
    /// actions (hovering + Space, clicking, its own menu) still work. Not
    /// persisted; cleared on restart.
    pub locked: bool,

    // Animated-GIF playback.
    frame: usize,
    anim_started: Option<f64>,
    /// User-paused GIF animation (click the middle to toggle).
    anim_paused: bool,
    /// Whether the item drawn last frame was animated (gates the pause click).
    last_animated: bool,
    /// GIF playback-speed multiplier (1.0 = 100%), reset per clip. Scales the
    /// per-frame delays; the video path uses mpv's own `speed` instead.
    gif_speed: f64,
    /// Text buffer for this pane's custom-speed field.
    pub speed_input: String,

    // Cached each draw so zoom/pan clamping can run without the image handy.
    last_base: Vec2,
}

impl Pane {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            folder: None,
            sort: SortOrder::default(),
            all_files: Vec::new(),
            type_filter: gallery::all_supported_exts(),
            search: String::new(),
            files: Vec::new(),
            cursor: 0,
            zoom: 1.0,
            pan: Vec2::ZERO,
            dialog_request: None,
            delete_armed: false,
            delete_target: None,
            delete_request: None,
            switch_request: None,
            loop_folder: false,
            last_nav: None,
            rotation: 0,
            excluded: HashSet::new(),
            list_manage_request: false,
            locked: false,
            lm_occupied: [false; 4],
            frame: 0,
            anim_started: None,
            anim_paused: false,
            last_animated: false,
            gif_speed: 1.0,
            speed_input: String::new(),
            last_base: Vec2::ZERO,
        }
    }

    // --- Folder / navigation ---------------------------------------------

    /// Load `folder`'s WHOLE subtree as the browse list (root files first,
    /// then subfolders — see `gallery::scan_folder`), starting at `start`.
    pub fn set_folder(&mut self, folder: PathBuf, start: Option<&Path>) {
        let files = gallery::scan_folder(&folder, self.sort);
        self.set_scanned(folder, files, start);
    }

    /// Adopt an already-scanned tree (folder drag-drop pre-checks the scan
    /// result before replacing this pane, so it isn't scanned twice).
    pub fn set_scanned(&mut self, folder: PathBuf, files: Vec<PathBuf>, start: Option<&Path>) {
        self.all_files = files;
        self.folder = Some(folder);
        self.excluded.clear(); // a fresh folder starts fully marked
        // A fresh folder also starts unfiltered.
        self.type_filter = gallery::all_supported_exts();
        self.search.clear();
        self.rebuild_filtered();
        self.cursor = match start {
            Some(s) => self.files.iter().position(|p| p == s).unwrap_or(0),
            None => 0,
        };
        self.reset_view();
    }

    /// True if `path` passes the current type + search filters.
    fn passes_filter(&self, path: &Path) -> bool {
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| self.type_filter.contains(&e.to_ascii_lowercase()))
            .unwrap_or(false);
        if !ext_ok {
            return false;
        }
        if self.search.is_empty() {
            return true;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase();
        name.contains(&self.search.to_lowercase())
    }

    fn rebuild_filtered(&mut self) {
        self.files = self
            .all_files
            .iter()
            .filter(|p| self.passes_filter(p))
            .cloned()
            .collect();
    }

    /// Re-apply the type/search filters after Gallery management changed them,
    /// keeping the current file if it survives the filter. If it doesn't, a
    /// DIFFERENT file lands under the cursor — reset the view so it isn't
    /// shown with the old file's zoom/pan/rotation (same as refresh_folder).
    pub fn apply_filter(&mut self) {
        let current = self.current_path().cloned();
        self.rebuild_filtered();
        match current.and_then(|c| self.files.iter().position(|p| *p == c)) {
            Some(i) => self.cursor = i,
            None => {
                self.cursor = self.cursor.min(self.files.len().saturating_sub(1));
                self.reset_view();
            }
        }
    }

    /// Adopt a selection dragged out of List Management: the source pane's
    /// folder and file ordering, with exactly `keep` ticked; the first kept
    /// file (in list order) becomes current.
    pub fn adopt_selection(&mut self, folder: PathBuf, files: Vec<PathBuf>, keep: &HashSet<PathBuf>) {
        self.folder = Some(folder);
        self.excluded = files.iter().filter(|p| !keep.contains(*p)).cloned().collect();
        self.cursor = files.iter().position(|p| keep.contains(p)).unwrap_or(0);
        self.all_files = files.clone();
        self.type_filter = gallery::all_supported_exts();
        self.search.clear();
        self.files = files;
        self.reset_view();
    }

    /// Re-scan the current folder in place (files were renamed/added/removed
    /// by the rename window), keeping the current file if it still exists.
    /// If it doesn't, a DIFFERENT file lands under the cursor — reset the
    /// view so it isn't shown with the old file's zoom/pan/rotation.
    pub fn refresh_folder(&mut self) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let current = self.current_path().cloned();
        self.all_files = gallery::scan_folder(&folder, self.sort);
        let listed: HashSet<&PathBuf> = self.all_files.iter().collect();
        self.excluded.retain(|p| listed.contains(p));
        // The type/search filters (and marks/selection) survive a refresh.
        self.rebuild_filtered();
        match current.and_then(|c| self.files.iter().position(|p| *p == c)) {
            Some(i) => self.cursor = i,
            None => {
                self.cursor = self.cursor.min(self.files.len().saturating_sub(1));
                self.reset_view();
            }
        }
    }

    pub fn resort(&mut self) {
        let current = self.current_path().cloned();
        if let Some(folder) = &self.folder {
            self.all_files = gallery::scan_folder(folder, self.sort);
            self.rebuild_filtered();
            self.cursor = current
                .and_then(|c| self.files.iter().position(|p| *p == c))
                .unwrap_or(0);
        }
    }

    pub fn clear(&mut self) {
        self.folder = None;
        self.files.clear();
        self.all_files.clear();
        self.excluded.clear();
        self.type_filter = gallery::all_supported_exts();
        self.search.clear();
        self.cursor = 0;
        self.reset_view();
    }

    pub fn current_path(&self) -> Option<&PathBuf> {
        self.files.get(self.cursor)
    }

    /// The complete scanned tree, before the type/search filters.
    pub fn all_files(&self) -> &[PathBuf] {
        &self.all_files
    }

    // --- Playlist (List Management ticks) ---------------------------------

    /// True if `path` is ticked (part of what this pane cycles through).
    pub fn is_included(&self, path: &Path) -> bool {
        !self.excluded.contains(path)
    }

    /// Adopt a saved un-marked set (Gallery management snapshots the marks
    /// around its duplicates view), dropping entries that left the list.
    pub fn restore_excluded(&mut self, excluded: HashSet<PathBuf>) {
        let listed: HashSet<&PathBuf> = self.all_files.iter().collect();
        self.excluded = excluded.into_iter().filter(|p| listed.contains(p)).collect();
    }

    /// Tick or un-tick one file. Never touches the disk — only which files
    /// navigation moves through.
    pub fn set_included(&mut self, path: &Path, included: bool) {
        if included {
            self.excluded.remove(path);
        } else {
            self.excluded.insert(path.to_path_buf());
        }
    }

    /// Jump straight to `path` (List-Management double-click). No-op if the
    /// file isn't in this pane's list.
    pub fn jump_to(&mut self, path: &Path) {
        if let Some(i) = self.files.iter().position(|p| p == path) {
            if i != self.cursor {
                self.cursor = i;
                self.reset_view();
            }
        }
    }

    fn next_included(&self, from: usize) -> Option<usize> {
        (from + 1..self.files.len()).find(|&i| self.is_included(&self.files[i]))
    }

    fn prev_included(&self, from: usize) -> Option<usize> {
        (0..from.min(self.files.len())).rev().find(|&i| self.is_included(&self.files[i]))
    }

    fn first_included(&self) -> Option<usize> {
        (0..self.files.len()).find(|&i| self.is_included(&self.files[i]))
    }

    fn last_included(&self) -> Option<usize> {
        (0..self.files.len()).rev().find(|&i| self.is_included(&self.files[i]))
    }

    /// Rotate the current item by `steps` clockwise quarter-turns (3 = one
    /// turn counter-clockwise). A turn invalidates the old zoom/pan framing.
    pub fn rotate(&mut self, steps: u8) {
        if self.current_path().is_none() {
            return;
        }
        self.rotation = (self.rotation + steps) % 4;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Set this GIF's playback-speed multiplier (1.0 = 100%), clamped 1%..500%.
    pub fn set_gif_speed(&mut self, mult: f64) {
        self.gif_speed = crate::widgets::clamp_speed(mult);
    }

    /// Freeze an animated GIF (pin-to-panel covers this pane; no-op otherwise).
    pub fn pause_anim(&mut self) {
        if self.last_animated {
            self.anim_paused = true;
        }
    }

    /// Resume a frozen GIF (unpin restores the pre-pin play state).
    pub fn resume_anim(&mut self) {
        if self.last_animated && self.anim_paused {
            self.anim_paused = false;
            self.anim_started = None; // resume from now, not the stale clock
        }
    }

    /// True while an animated GIF is actually advancing (captured at pin time
    /// so unpinning can restore playing/paused exactly as it was).
    pub fn anim_playing(&self) -> bool {
        self.last_animated && !self.anim_paused
    }

    /// Pause/resume an animated GIF (no-op for other content).
    pub fn toggle_anim_pause(&mut self) {
        if self.last_animated {
            self.anim_paused = !self.anim_paused;
            if !self.anim_paused {
                // Resume from now, not from the stale frame clock.
                self.anim_started = None;
            }
        }
    }

    /// Drop `path` from the browse list (after it was deleted from disk),
    /// keeping the cursor on a sensible neighbour.
    pub fn remove_file(&mut self, path: &Path) {
        self.excluded.remove(path);
        if let Some(i) = self.all_files.iter().position(|p| p == path) {
            self.all_files.remove(i);
        }
        let Some(i) = self.files.iter().position(|p| p == path) else {
            return;
        };
        self.files.remove(i);
        if self.files.is_empty() {
            self.cursor = 0;
            self.reset_view();
            return;
        }
        if i < self.cursor {
            self.cursor -= 1; // same file stays current
        } else if i == self.cursor {
            if self.cursor >= self.files.len() {
                self.cursor = self.files.len() - 1;
            }
            self.reset_view(); // a different file is now current
        }
    }

    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.rotation = 0;
        self.frame = 0;
        self.anim_started = None;
        self.anim_paused = false;
        self.gif_speed = 1.0;
    }

    pub fn next(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if let Some(i) = self.next_included(self.cursor) {
            self.cursor = i;
            self.reset_view();
        } else if self.loop_folder {
            // Wrap to the first TICKED file (may be the current one when it is
            // the only tick — same feel as looping a single-file folder).
            if let Some(i) = self.first_included() {
                self.cursor = i;
                self.reset_view();
                self.last_nav = Some(NavFlash::Wrapped);
            } else {
                // Nothing ticked at all: navigation has nowhere to go.
                self.last_nav = Some(NavFlash::Blocked);
            }
        } else {
            // At the last ticked file with looping off: stay and flag the block.
            self.last_nav = Some(NavFlash::Blocked);
        }
    }

    pub fn prev(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if let Some(i) = self.prev_included(self.cursor) {
            self.cursor = i;
            self.reset_view();
        } else if self.loop_folder {
            if let Some(i) = self.last_included() {
                self.cursor = i;
                self.reset_view();
                self.last_nav = Some(NavFlash::Wrapped);
            } else {
                self.last_nav = Some(NavFlash::Blocked);
            }
        } else {
            self.last_nav = Some(NavFlash::Blocked);
        }
    }

    fn preload(&self, store: &mut ImageStore) {
        let n = self.files.len();
        if n == 0 {
            return;
        }
        let mut req = |i: usize| {
            let p = &self.files[i];
            if gallery::is_image(p) {
                store.request(p);
            }
        };
        req(self.cursor % n);
        // Preload navigation's REAL neighbours — the next/previous ticked
        // files (wrapping like `next`/`prev` do when looping is on).
        match self.next_included(self.cursor) {
            Some(i) => req(i),
            None if self.loop_folder => {
                if let Some(i) = self.first_included() {
                    req(i);
                }
            }
            None => {}
        }
        match self.prev_included(self.cursor) {
            Some(i) => req(i),
            None if self.loop_folder => {
                if let Some(i) = self.last_included() {
                    req(i);
                }
            }
            None => {}
        }
    }

    // --- Zoom / pan ------------------------------------------------------

    /// Zoom keeping the point under the cursor fixed (ctrl+scroll).
    pub fn zoom_at(&mut self, rect: Rect, cursor: Pos2, factor: f32) {
        let old = self.zoom;
        let new = (old * factor).clamp(1.0, 16.0);
        if (new - old).abs() < f32::EPSILON {
            return;
        }
        let origin = rect.center().to_vec2();
        let c = cursor.to_vec2();
        // Keep the world point under the cursor stationary on screen.
        self.pan = (c - origin) - ((c - origin) - self.pan) * (new / old);
        self.zoom = new;
        if self.zoom <= 1.0001 {
            self.pan = Vec2::ZERO;
        }
        self.clamp_pan(rect);
    }

    /// Pan vertically (scroll-to-pan when zoomed in), clamped.
    pub fn pan_scroll(&mut self, rect: Rect, dy: f32) {
        self.pan.y += dy;
        self.clamp_pan(rect);
    }

    fn clamp_pan(&mut self, rect: Rect) {
        let scaled = self.last_base * self.zoom;
        let mx = ((scaled.x - rect.width()) * 0.5).max(0.0);
        let my = ((scaled.y - rect.height()) * 0.5).max(0.0);
        self.pan.x = self.pan.x.clamp(-mx, mx);
        self.pan.y = self.pan.y.clamp(-my, my);
    }

    // --- Rendering + interaction ----------------------------------------

    pub fn show(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        store: &mut ImageStore,
        rect: Rect,
        bg: Color32,
        suppress: bool,
    ) {
        let resp = ui.interact(rect, Id::new(("mulvie_pane", self.index)), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);

        self.preload(store);

        if let Some(path) = self.files.get(self.cursor).cloned() {
            if let Some(img) = store.get(&path) {
                self.draw_image(ctx, &painter, rect, &img);
            } else if store.is_failed(&path) {
                center_text(&painter, rect, "⚠  cannot open this image", theme::HINT);
            } else {
                center_text(&painter, rect, "loading…", theme::HINT);
                ctx.request_repaint();
            }
        }
        // Empty pane (no folder, or folder with no media) is left blank.

        // Click zones vs drag-to-pan.
        if self.zoom > 1.0001 && resp.dragged() {
            self.pan += resp.drag_delta();
            self.clamp_pan(rect);
        } else if resp.clicked() && !suppress {
            // A blank pane does nothing on click (open a file via the
            // right-click menu or drag-and-drop). On a loaded pane the side
            // strips navigate and the middle pauses/resumes a GIF.
            if let Some(pos) = resp.interact_pointer_pos() {
                if self.current_path().is_some() {
                    let x = (pos.x - rect.left()) / rect.width();
                    if x < SIDE {
                        self.prev();
                    } else if x > 1.0 - SIDE {
                        self.next();
                    } else {
                        self.toggle_anim_pause();
                    }
                }
            }
        }

        // Right-click menu.
        resp.context_menu(|ui| self.context_menu(ui));
    }

    fn draw_image(&mut self, ctx: &Context, painter: &Painter, rect: Rect, img: &LoadedImage) {
        self.last_animated = img.is_animated();
        let frame_index = if img.is_animated() && !self.anim_paused {
            let now = ctx.input(|i| i.time);
            let started = *self.anim_started.get_or_insert(now);
            let idx = self.frame.min(img.frames.len() - 1);
            // Scale each frame's delay by the speed multiplier: faster speed =
            // shorter on-screen delay. `gif_speed` is clamped >= 0.01 so this
            // never divides by zero.
            let delay = img.frames[idx].delay.max(0.02) as f64 / self.gif_speed;
            let next_idx = if now - started >= delay {
                let n = (idx + 1) % img.frames.len();
                self.frame = n;
                self.anim_started = Some(now);
                n
            } else {
                idx
            };
            let elapsed = now - self.anim_started.unwrap_or(now);
            let remaining =
                (img.frames[next_idx].delay.max(0.02) as f64 / self.gif_speed - elapsed).max(0.0);
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
            next_idx
        } else if img.is_animated() {
            // Paused: hold the current frame, no repaint scheduling.
            self.frame.min(img.frames.len() - 1)
        } else {
            0
        };

        let (iw, ih) = (img.size[0] as f32, img.size[1] as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        // A quarter-turn swaps the axes the image occupies on screen.
        let (rw, rh) = if self.rotation % 2 == 1 { (ih, iw) } else { (iw, ih) };
        let fit = (rect.width() / rw).min(rect.height() / rh);
        self.last_base = vec2(rw * fit, rh * fit);
        let scaled = self.last_base * self.zoom;
        let img_rect = Rect::from_center_size(rect.center() + self.pan, scaled);
        let tex_id = img.frames[frame_index].tex.id();
        if self.rotation == 0 {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(tex_id, img_rect, uv, Color32::WHITE);
        } else {
            // Rotated draw: same rect, texture corners permuted by k quarter
            // turns clockwise (corner i shows texture corner (i + 4 - k) % 4).
            let ps = [
                img_rect.left_top(),
                img_rect.right_top(),
                img_rect.right_bottom(),
                img_rect.left_bottom(),
            ];
            let base = [
                pos2(0.0, 0.0),
                pos2(1.0, 0.0),
                pos2(1.0, 1.0),
                pos2(0.0, 1.0),
            ];
            let k = self.rotation as usize % 4;
            let mut mesh = egui::Mesh::with_texture(tex_id);
            for (i, &pos) in ps.iter().enumerate() {
                mesh.vertices.push(egui::epaint::Vertex {
                    pos,
                    uv: base[(i + 4 - k) % 4],
                    color: Color32::WHITE,
                });
            }
            mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
            painter.add(egui::Shape::mesh(mesh));
        }
    }

    /// The shared open + sort + switch + delete items, reused by the
    /// image/video/PDF menus.
    pub fn folder_menu_items(&mut self, ui: &mut Ui) {
        if ui.button("Open file…").clicked() {
            self.dialog_request = Some(DialogMode::File);
            ui.close_menu();
        }
        if ui.button("Gallery management").clicked() {
            self.list_manage_request = true;
            ui.close_menu();
        }

        ui.separator();
        ui.menu_button("Sort by", |ui| {
            for order in SortOrder::ALL {
                if ui.radio(self.sort == order, order.label()).clicked() {
                    if self.sort != order {
                        self.sort = order;
                        self.resort();
                    }
                    ui.close_menu();
                }
            }
        });
        // Panels read A B C D: top row A B, bottom row C D.
        ui.menu_button("Switch panel content", |ui| {
            const LABELS: [&str; 4] = ["Panel A", "Panel B", "Panel C", "Panel D"];
            const ORDER: [usize; 4] = [0, 2, 1, 3]; // A=TL, B=TR, C=BL, D=BR
            for (label, &target) in LABELS.iter().zip(ORDER.iter()) {
                // A panel occupied by pinned List Management takes no content.
                let occupied = self.lm_occupied[target];
                if ui
                    .add_enabled(target != self.index && !occupied, egui::Button::new(*label))
                    .clicked()
                {
                    self.switch_request = Some(target);
                    ui.close_menu();
                }
            }
        });

        // Delete, two-step: the entry re-arms into an "Are you sure ▸ Yes"
        // submenu; anything that closes the menu (click elsewhere, Esc)
        // cancels — the app clears `delete_armed` when no menu is open.
        ui.separator();
        if self.delete_armed {
            let name = self
                .delete_target
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("this file")
                .to_owned();
            ui.menu_button("Are you sure?", |ui| {
                ui.label(egui::RichText::new(&name).color(theme::INK_BLUE).size(11.0));
                if ui.button("Yes, delete").clicked() {
                    // Delete the file captured when armed, not the pane's
                    // current file (which may have changed since).
                    self.delete_request = self.delete_target.take();
                    self.delete_armed = false;
                    ui.close_menu();
                }
            });
        } else if ui
            .add_enabled(
                self.current_path().is_some(),
                egui::Button::new("Delete file"),
            )
            .clicked()
        {
            self.delete_target = self.current_path().cloned(); // capture now
            self.delete_armed = true; // menu stays open, entry becomes confirm
        }
    }

    pub fn context_menu(&mut self, ui: &mut Ui) {
        // Snug to the widest item ("Switch panel content"); this floor also
        // fixes the row width so the rotate pair stays flush right.
        ui.set_min_width(crate::widgets::MENU_WIDTH);
        let has = self.current_path().is_some();
        let row = crate::widgets::nav_rotate_row(ui, has);
        if row.prev {
            self.prev();
            ui.close_menu();
        }
        if row.next {
            self.next();
            ui.close_menu();
        }
        // Rotate deliberately does NOT close the menu (90° per click).
        if row.cw {
            self.rotate(1);
        }
        if row.ccw {
            self.rotate(3);
        }
        ui.separator();
        self.folder_menu_items(ui);

        // Lock: exclude this pane from the global batch commands.
        ui.separator();
        let lock_label = if self.locked { "Unlock panel" } else { "Lock panel" };
        if ui.button(lock_label).clicked() {
            self.locked = !self.locked;
            ui.close_menu();
        }

        // Speed control for animated GIFs (mirrors the video "Speed" submenu).
        if self.last_animated {
            ui.separator();
            let cur = self.gif_speed;
            let mut chosen = None;
            ui.menu_button("Speed", |ui| {
                chosen = crate::widgets::speed_menu(ui, cur, &mut self.speed_input);
            });
            if let Some(m) = chosen {
                self.set_gif_speed(m);
            }
        }

        ui.separator();
        if ui
            .add_enabled(self.zoom > 1.0001, egui::Button::new("Reset zoom"))
            .clicked()
        {
            // "Reset zoom" restores framing only; keep the chosen GIF speed
            // (reset_view clears it, but the video path preserves speed here).
            let keep_speed = self.gif_speed;
            self.reset_view();
            self.gif_speed = keep_speed;
            ui.close_menu();
        }
        if ui
            .add_enabled(self.folder.is_some(), egui::Button::new("Clear the panel"))
            .clicked()
        {
            self.clear();
            ui.close_menu();
        }
    }
}

fn center_text(painter: &Painter, rect: Rect, text: &str, color: Color32) {
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(14.0),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::Pane;
    use std::path::PathBuf;

    fn pane_with(names: &[&str], cursor: usize) -> Pane {
        let mut p = Pane::new(0);
        p.files = names.iter().map(PathBuf::from).collect();
        p.folder = Some(PathBuf::from("x"));
        p.cursor = cursor;
        p
    }

    /// Deleting keeps the cursor on a sensible neighbour.
    #[test]
    fn remove_file_cursor_math() {
        // Delete before the cursor: same file stays current.
        let mut p = pane_with(&["a", "b", "c"], 2);
        p.remove_file(&PathBuf::from("a"));
        assert_eq!(p.cursor, 1);
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("c"));

        // Delete the current (middle): next file becomes current.
        let mut p = pane_with(&["a", "b", "c"], 1);
        p.remove_file(&PathBuf::from("b"));
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("c"));

        // Delete the current (last): clamps to the new last.
        let mut p = pane_with(&["a", "b", "c"], 2);
        p.remove_file(&PathBuf::from("c"));
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("b"));

        // Delete the only file: list empties cleanly.
        let mut p = pane_with(&["a"], 0);
        p.remove_file(&PathBuf::from("a"));
        assert!(p.current_path().is_none());
        assert_eq!(p.cursor, 0);

        // Deleting something not in the list is a no-op.
        let mut p = pane_with(&["a", "b"], 1);
        p.remove_file(&PathBuf::from("zz"));
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("b"));
    }

    /// Navigation moves only through TICKED files; the current file stays
    /// visible when un-ticked until the user navigates; zero ticks = no-op.
    #[test]
    fn nav_skips_unticked_files() {
        use super::NavFlash;
        use std::path::Path;

        // b and c un-ticked: from a, next lands on d, prev returns to a.
        let mut p = pane_with(&["a", "b", "c", "d"], 0);
        p.set_included(Path::new("b"), false);
        p.set_included(Path::new("c"), false);
        p.next();
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("d"));
        p.prev();
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("a"));

        // Loop off at the last ticked file: blocked, cursor stays.
        let mut p = pane_with(&["a", "b", "c"], 0);
        p.set_included(Path::new("b"), false);
        p.set_included(Path::new("c"), false);
        p.next();
        assert_eq!(p.cursor, 0);
        assert_eq!(p.last_nav, Some(NavFlash::Blocked));

        // Loop on: wraps to the FIRST ticked file, skipping un-ticked a.
        let mut p = pane_with(&["a", "b", "c"], 2);
        p.loop_folder = true;
        p.set_included(Path::new("a"), false);
        p.next();
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("b"));
        assert_eq!(p.last_nav, Some(NavFlash::Wrapped));

        // Current file un-ticked: stays current until nav, which skips it.
        let mut p = pane_with(&["a", "b", "c"], 1);
        p.set_included(Path::new("b"), false);
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("b")); // still shown
        p.next();
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("c"));

        // Nothing ticked: navigation has nowhere to go, loop on or off.
        let mut p = pane_with(&["a", "b"], 0);
        p.loop_folder = true;
        p.set_included(Path::new("a"), false);
        p.set_included(Path::new("b"), false);
        p.next();
        assert_eq!(p.cursor, 0);
        assert_eq!(p.last_nav, Some(NavFlash::Blocked));
        p.prev();
        assert_eq!(p.cursor, 0);
    }

    /// A drag-drop from List Management: the target pane adopts the folder +
    /// ordering with exactly the dragged files ticked, showing the first one.
    #[test]
    fn adopt_selection_builds_the_dropped_subset() {
        use std::collections::HashSet;
        let files: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(PathBuf::from).collect();
        let keep: HashSet<PathBuf> = [PathBuf::from("b"), PathBuf::from("d")].into();
        let mut p = Pane::new(2);
        p.adopt_selection(PathBuf::from("dir"), files, &keep);
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("b"));
        let marked = p.files.iter().filter(|f| p.is_included(f)).count();
        assert_eq!(marked, 2);
        p.next();
        assert_eq!(p.current_path().unwrap(), &PathBuf::from("d"));
        // a and c are listed but skipped.
        assert!(!p.is_included(std::path::Path::new("a")));
        assert_eq!(p.files.len(), 4);
    }

    /// Double-click in List Management jumps the pane to that file.
    #[test]
    fn jump_to_moves_cursor() {
        let mut p = pane_with(&["a", "b", "c"], 0);
        p.jump_to(std::path::Path::new("c"));
        assert_eq!(p.cursor, 2);
        p.jump_to(std::path::Path::new("zz")); // unknown file: no-op
        assert_eq!(p.cursor, 2);
    }
}
