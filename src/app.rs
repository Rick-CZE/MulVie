//! Top-level application: chrome, layout orchestration, global input routing
//! (scroll = browse, ctrl+scroll = zoom, alt+scroll = all panes), divider
//! dragging, and config persistence.

use eframe::egui::{
    self, pos2, Align, Color32, Context, CursorIcon, Id, Key, Rect, RichText, Sense, Stroke,
};

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::config::Config;
use crate::file_ops;
use crate::gallery;
use crate::image_store::ImageStore;
use crate::layout::{Layout, Quadrant};
use crate::list_manager::{self, ListManager};
use crate::pane::{DialogMode, NavFlash, Pane};
use crate::pdf::{self, PdfView};
use crate::theme;
use crate::thumbs::ThumbStore;
use crate::video::{VideoPlayer, Visual};
use crate::widgets::{self, Icon};
use pdfium_render::prelude::Pdfium;

const HEADER_HEIGHT: f32 = 34.0;
const DIVIDER_GRAB: f32 = 10.0;
const DIVIDER_THICK: f32 = 3.0;
/// A divider this close to a window edge is practically unpickable — the edge
/// resize band (or the header) sits on top of most of its grab zone.
const REVEAL_NEAR_EDGE: f32 = 12.0;
/// While TAB is held, such a divider is drawn (and grabbed) this far in from
/// the edge, so it can be caught and dragged back.
const REVEAL_NUDGE: f32 = 16.0;
/// How many pixels of touchpad scroll equal one "notch" (one image step).
const POINT_PER_STEP: f32 = 40.0;
/// Pixels of vertical pan per scroll notch when the pane is zoomed in.
const PAN_STEP: f32 = 80.0;
/// Fraction of a pane's width (each side) that navigates prev/next on click.
const IMG_SIDE: f32 = 0.18;
/// Shell parsing name of the "This PC" virtual folder — where file dialogs
/// start on the first open of each session (SHCreateItemFromParsingName
/// resolves this CLSID moniker; rfd passes it through untouched).
pub(crate) const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// What this platform calls its OS trash, for user-facing prompts.
#[cfg(windows)]
const TRASH_NAME: &str = "Recycle Bin";
#[cfg(not(windows))]
const TRASH_NAME: &str = "Trash";

/// What a drag that started on a video pane is controlling. Latched at
/// drag-start so e.g. panning a zoomed video doesn't get hijacked by the
/// seek/volume bars when the pointer passes over them mid-drag.
#[derive(Clone, Copy, PartialEq)]
enum VidDrag {
    Seek,
    Volume,
    Pan,
}
/// What an off-thread native picker was opened FOR (routes its result).
enum DialogKind {
    /// "Open file…" for a pane: the picked file's parent tree loads.
    OpenFile { pane: usize },
    /// "Open folder…" (List Management empty state) for a pane.
    OpenFolder { pane: usize },
    /// Destination folder for a move-marked batch; `owner` = the requesting
    /// browser's slot (its counts row carries the progress/result message).
    MoveDest { files: Vec<PathBuf>, owner: usize },
}

/// One native picker at a time; a repeat request FOCUSES the open picker.
/// (Unparented pickers could sit unnoticed behind the windows while every
/// further request was silently dropped — the "Open folder does nothing" bug.
/// They are parented to the main window now, and this is the second belt.)
struct PendingDialog {
    kind: DialogKind,
    title: &'static str,
    rx: Receiver<Option<PathBuf>>,
}

/// A "library content missing" notice for one panel: shown after a library
/// load when the stored folder (folder_gone) or file no longer exists. The
/// panel pulses red twice, the missing path is printed in its middle, a left
/// click dismisses it and right-click offers copying the path. The library
/// itself is never modified — reloading it shows the notice again until the
/// user re-writes it by hand.
struct LibMissing {
    /// The path that no longer exists (the folder or the file).
    path: PathBuf,
    /// True = the whole folder is gone (panel stays empty); false = only the
    /// file is gone (the folder's first file was loaded instead).
    folder_gone: bool,
    /// Set on the first visible frame, so the pulse starts when it can be seen.
    pulse_start: Option<f64>,
}

/// A move-marked batch in progress: files move one by one, pausing on a name
/// conflict for the in-app prompt (Cancel / Replace / Keep both, optionally
/// for all).
struct MoveBatch {
    dest: PathBuf,
    queue: std::collections::VecDeque<PathBuf>,
    /// The source file whose destination name is taken, awaiting a choice.
    conflict: Option<PathBuf>,
    apply_all: Option<ConflictChoice>,
    /// The "Do this for all conflicts" checkbox state in the prompt.
    apply_all_checkbox: bool,
    /// The requesting browser's slot: progress/result messages go to it only.
    owner: usize,
    /// Queue length at start, for the "Moving k of n…" progress line.
    total: usize,
    moved: usize,
    replaced: usize,
    kept_both: usize,
    skipped: usize,
    failed: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum ConflictChoice {
    Replace,
    KeepBoth,
    Skip,
}

/// A pending confirmation for a destructive library action.
enum LibConfirm {
    /// Overwrite the named library with the current panel state.
    Rewrite(String),
    /// Delete the named library (the content it points at is untouched).
    Delete(String),
}

/// A borrowed HWND as a raw-window-handle provider so rfd can parent its
/// dialogs to the main window — parented pickers always open in front.
struct ParentWindow(isize);

impl raw_window_handle::HasWindowHandle for ParentWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let nz = std::num::NonZeroIsize::new(self.0)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let raw = raw_window_handle::RawWindowHandle::Win32(
            raw_window_handle::Win32WindowHandle::new(nz),
        );
        // SAFETY: the main window outlives every picker thread's short use.
        unsafe { Ok(raw_window_handle::WindowHandle::borrow_raw(raw)) }
    }
}

impl raw_window_handle::HasDisplayHandle for ParentWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let raw = raw_window_handle::RawDisplayHandle::Windows(
            raw_window_handle::WindowsDisplayHandle::new(),
        );
        unsafe { Ok(raw_window_handle::DisplayHandle::borrow_raw(raw)) }
    }
}

pub struct MulVieApp {
    layout: Layout,
    panes: [Pane; 4],
    store: ImageStore,
    fullscreen: bool,
    show_dividers: bool,
    glass: bool,
    glass_tried: bool,
    /// One lazily-created video player per pane (reused across clips).
    videos: [Option<VideoPlayer>; 4],
    /// Per-pane audio mute (default muted so the user unmutes one at a time).
    muted: [bool; 4],
    /// pdfium binding (None if pdfium.dll isn't next to the exe).
    pdfium: Option<Pdfium>,
    /// Per-pane PDF viewer state.
    pdfs: [Option<PdfView>; 4],
    /// Presentation curtain: frost hides every pane (and the dividers), showing
    /// only the app background, so the audience can't see what's preloaded.
    frost_all: bool,
    /// Autoplay new videos/audio (persisted). OFF → every clip starts paused.
    autoplay: bool,
    /// Launch unmuted (persisted). Applied once at startup to `muted`.
    sound_on_startup: bool,
    /// Launch in the four-panel view when opened empty (persisted).
    four_panel_default: bool,
    /// Presentation cover freezes content (persisted). When on, raising the
    /// cover pauses everything and lifting it resumes what was playing.
    cover_freezes: bool,
    /// Which panes the cover paused (were playing when it froze), so lifting
    /// the cover resumes exactly those.
    cover_playing: [bool; 4],
    /// Which divider (0=vertical, 1=left-h, 2=right-h) was being dragged LAST
    /// frame — so the ALT edge-nudge leaves a divider the user is holding alone.
    div_dragging: [bool; 3],
    /// Background tint colour, held as HSVA (the picker's native type) so the
    /// square/hue picker edits it directly with no per-frame Color32 round-trip
    /// (which egui's `color_picker_color32` drifts to black). Combined with
    /// `bg_alpha` for the real tint.
    bg_hsva: egui::ecolor::Hsva,
    /// Background opacity: 0 = fully see-through frosted glass, 255 = solid colour.
    bg_alpha: u8,
    /// Gallery-management name colour, held as HSVA like `bg_hsva` (the picker
    /// edits it directly with no drifting Color32 round-trip).
    text_hsva: egui::ecolor::Hsva,
    /// Last tint (ABGR) pushed to the acrylic layer, so we only re-apply on change.
    applied_abgr: Option<u32>,
    /// The "MulVie" dropdown menu (background tint + moved toggles) open state.
    bg_menu_open: bool,
    /// Whether the MulVie menu's Settings section is expanded (startup/playback
    /// toggles + colour palettes). Mutually exclusive with the Library section.
    bg_menu_colors: bool,
    /// Whether the MulVie menu's Library section is expanded.
    bg_menu_library: bool,
    /// One-shot: reset the menu section's scroll to the top on the next draw.
    /// Set when the menu opens and when a section is switched on — egui keeps
    /// scroll offsets forever, so without this a bottom-scrolled Settings
    /// (e.g. after clicking About in a short window) would REOPEN scrolled to
    /// the bottom, hiding the toggles with only the faint handle as a clue.
    menu_scroll_reset: bool,
    /// Saved libraries (see config::Library) + the section's UI state.
    libraries: Vec<crate::config::Library>,
    lib_search: String,
    /// The selected library's name (survives list re-filtering).
    lib_selected: Option<String>,
    /// Height of the Settings section last time it was drawn, so the Library
    /// section can pad its list to the SAME total height (identical menus).
    menu_section_h: f32,
    /// The last library save/rename/delete failed to reach the disk
    /// (read-only or yanked stick) — shown as a warning in the section.
    lib_save_error: bool,
    /// Per-panel "library content missing" notices (see [`LibMissing`]).
    lib_missing: [Option<LibMissing>; 4],
    /// The About window (opened from the bottom of the Settings section).
    about_open: bool,
    /// Its own acrylic state — a fresh native window needs glass re-applied
    /// on every (re)open, found via its unique OS title (the documented
    /// child-viewport pattern).
    about_glass: bool,
    about_glass_attempts: u32,
    /// The tint the About acrylic was last applied with — re-applied when the
    /// user drags the colour/opacity sliders while it is open, so it retints
    /// live like the main and Gallery-management windows do.
    about_applied_abgr: Option<u32>,
    /// Where the About window was centred when it OPENED. Kept constant while
    /// it stays open: egui re-sends a changed builder position to a live
    /// immediate viewport (ViewportBuilder::patch), so recomputing the centre
    /// every frame would teleport the window whenever the main window moves —
    /// and snap back a window the user deliberately dragged elsewhere.
    about_pos: Option<egui::Pos2>,
    /// Whether the keyboard focus (e.g. the library name field) was active
    /// LAST frame — egui clears focus while processing Escape, so this is the
    /// only reliable "was the user typing?" gate for the menu's Esc handling.
    menu_kb_focus_prev: bool,
    /// A rename in progress: (old name, editable new name).
    lib_rename: Option<(String, String)>,
    /// A pending confirm prompt for a library action.
    lib_confirm: Option<LibConfirm>,
    /// Screen-space rect of the wordmark/logo, so a click ANYWHERE else in the
    /// header closes the menu (but the wordmark itself keeps toggling it).
    brand_rect: Rect,
    /// Screen-space bottom-left of the wordmark, where the dropdown anchors.
    menu_anchor: egui::Pos2,
    /// True when fullscreen auto-hid the dividers, so leaving fullscreen can
    /// restore them (unless the user toggled dividers meanwhile).
    dividers_auto_hidden: bool,
    /// Text buffer for the custom-loop seconds input.
    loop_input: String,
    /// The one running off-thread native picker, if any.
    dialog: Option<PendingDialog>,
    /// Latched: swallow the click that dismisses an open context menu so it
    /// does nothing else. Set on the dismissing press, cleared on the next
    /// ordinary press (see `update`).
    suppress_clicks: bool,
    /// Whether a popup was open on the previous frame (for the latch above).
    popup_open_prev: bool,
    /// Global loop-folder toggle (wrap last→first); persisted, off by default.
    loop_enabled: bool,
    /// Auto-hide the cursor over content after 4s idle; persisted, ON by default.
    mouse_hide: bool,
    /// True this frame if the cursor is currently hidden (idle over content) —
    /// read by the video pane to also hide its hover chrome.
    cursor_hidden: bool,
    /// Last applied Windows "stay awake" request `(system, display)`, so we only
    /// call the OS when it changes. See `update_keep_awake`.
    keep_awake: (bool, bool),
    /// Last seen pointer position + the time it last moved, for idle detection.
    last_pointer_pos: Option<egui::Pos2>,
    last_pointer_move: f64,
    /// Per-pane transient boundary overlay: (kind, start time in seconds).
    nav_anim: [Option<(NavFlash, f64)>; 4],
    /// The List-Management browsers: up to four instances, each in its own
    /// window or pinned into a panel; any panel is managed by at most one.
    list_mgrs: Vec<ListManager>,
    /// The chrome toggle HIDES/SHOWS every instance (state kept); closing an
    /// instance for real is its own X button.
    lm_hidden: bool,
    /// Start time of the "can't open a fifth browser" red glow on the icon.
    lm_flash: Option<f64>,
    /// Where the chrome toggle sits this frame (the glow is drawn over it).
    lm_icon_rect: Rect,
    /// RAM-only thumbnail cache for List Management (never Windows' thumbcache).
    thumbs: ThumbStore,
    /// Per-pane one-shot: apply this `user_paused` to the next clip loaded
    /// into the pane's player. Set on unpin when the pane's file changed
    /// while it was covered — unpinning restores the play-state captured at
    /// pin time (playing resumes, paused stays paused).
    load_pause_state: [Option<bool>; 4],
    /// A running move-marked batch (may pause on a name-conflict prompt).
    move_batch: Option<MoveBatch>,
    /// "Delete N marked files?" confirmation payload + requesting slot.
    lm_delete_confirm: Option<(Vec<PathBuf>, usize)>,
    /// A confirmed delete batch draining one file per frame (so a big batch
    /// never freezes the UI): (queue, total, owner slot).
    lm_delete_queue: Option<(std::collections::VecDeque<PathBuf>, usize, usize)>,
    /// Per-pane latched video drag mode (seek / volume / pan) for the whole
    /// duration of one drag gesture.
    video_drag: [Option<VidDrag>; 4],
    /// Keyboard-delete confirmation: (captured file, where to open the dialog).
    delete_confirm: Option<(PathBuf, egui::Pos2)>,
    /// Main window handle (captured once; used for drop-position + focus).
    hwnd: Option<isize>,
    /// The real app icon as an egui texture (header logo = taskbar icon).
    logo_tex: Option<egui::TextureHandle>,
    /// Last folder a file dialog was used in — remembered for THIS session
    /// only. First dialog of a session starts at "This PC". Shared with the
    /// rename window.
    session_dir: Arc<Mutex<Option<PathBuf>>>,

    // Touchpad scroll accumulators (one per pane, plus one for alt=all).
    step_accum: [f32; 4],
    alt_accum: f32,

    // Debounced config saving.
    last_saved_json: String,
    last_save_time: f64,
}

impl MulVieApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        open_file: Option<PathBuf>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        // Pull in system fonts so non-Latin filenames and symbols render instead
        // of showing boxes (robust: missing/unrecognised fonts are skipped).
        crate::fonts::install_system_fallbacks(&cc.egui_ctx);
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);

        // Panes always start empty (nothing is remembered across restarts).
        // If launched with a file ("Open with MulVie"), it goes to the top-left.
        let mut panes: [Pane; 4] = std::array::from_fn(Pane::new);
        if let Some(file) = &open_file {
            if let Some(parent) = file.parent() {
                panes[0].set_folder(parent.to_path_buf(), Some(file));
            }
        }

        // The real app icon (same art as the taskbar) for the header logo.
        let logo_tex = image::load_from_memory(include_bytes!("../assets/icon.png"))
            .ok()
            .map(|img| {
                let img = img.to_rgba8();
                let (w, h) = img.dimensions();
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    img.as_raw(),
                );
                cc.egui_ctx
                    .load_texture("mulvie_logo", ci, egui::TextureOptions::LINEAR)
            });

        let session_dir: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

        Self {
            // Layout on start: a single top-left panel, unless the user set the
            // four-panel default AND no file was passed (a file always opens
            // single, showing that file).
            layout: if cfg.four_panel_default && open_file.is_none() {
                Layout::new(0.5, 0.5, 0.5)
            } else {
                Layout::new(1.0, 1.0, 1.0)
            },
            panes,
            store: ImageStore::new(),
            fullscreen: false,
            show_dividers: true,
            glass: false,
            glass_tried: false,
            videos: Default::default(),
            // Muted on start unless the user turned on sound-at-startup.
            muted: [!cfg.sound_on_startup; 4],
            pdfium: pdf::init(),
            pdfs: Default::default(),
            frost_all: false,
            autoplay: cfg.autoplay,
            sound_on_startup: cfg.sound_on_startup,
            four_panel_default: cfg.four_panel_default,
            cover_freezes: cfg.cover_freezes,
            cover_playing: [false; 4],
            div_dragging: [false; 3],
            bg_hsva: egui::ecolor::Hsva::from(Color32::from_rgb(
                cfg.bg_color[0],
                cfg.bg_color[1],
                cfg.bg_color[2],
            )),
            bg_alpha: cfg.bg_alpha,
            text_hsva: egui::ecolor::Hsva::from(Color32::from_rgb(
                cfg.text_color[0],
                cfg.text_color[1],
                cfg.text_color[2],
            )),
            applied_abgr: None,
            bg_menu_open: false,
            bg_menu_colors: false,
            bg_menu_library: false,
            menu_scroll_reset: false,
            libraries: crate::config::load_libraries(),
            lib_search: String::new(),
            lib_selected: None,
            menu_section_h: 0.0,
            lib_save_error: false,
            lib_missing: Default::default(),
            about_open: false,
            about_glass: false,
            about_glass_attempts: 0,
            about_applied_abgr: None,
            about_pos: None,
            menu_kb_focus_prev: false,
            lib_rename: None,
            lib_confirm: None,
            brand_rect: Rect::NOTHING,
            menu_anchor: egui::pos2(10.0, HEADER_HEIGHT),
            dividers_auto_hidden: false,
            loop_input: String::new(),
            dialog: None,
            suppress_clicks: false,
            popup_open_prev: false,
            loop_enabled: cfg.loop_enabled,
            mouse_hide: cfg.mouse_hide,
            cursor_hidden: false,
            keep_awake: (false, false),
            last_pointer_pos: None,
            last_pointer_move: 0.0,
            nav_anim: [None; 4],
            list_mgrs: Vec::new(),
            lm_hidden: false,
            lm_flash: None,
            lm_icon_rect: Rect::NOTHING,
            thumbs: ThumbStore::new(),
            load_pause_state: [None; 4],
            move_batch: None,
            lm_delete_confirm: None,
            lm_delete_queue: None,
            video_drag: [None; 4],
            delete_confirm: None,
            hwnd: None,
            logo_tex,
            session_dir,
            step_accum: [0.0; 4],
            alt_accum: 0.0,
            last_saved_json: String::new(),
            last_save_time: 0.0,
        }
    }

    fn set_fullscreen(&mut self, ctx: &Context, on: bool) {
        if on {
            // No wordmark to anchor the dropdown to once the chrome is gone.
            self.bg_menu_open = false;
            // With a single active panel the only divider lines are stray edge
            // strokes; hide them for a clean fullscreen and remember to restore.
            let single = self.layout.pane_rects(self.content_area(ctx)).len() <= 1;
            if single && self.show_dividers {
                self.show_dividers = false;
                self.dividers_auto_hidden = true;
            }
        } else if self.dividers_auto_hidden {
            self.show_dividers = true;
            self.dividers_auto_hidden = false;
        }
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    fn toggle_fullscreen(&mut self, ctx: &Context) {
        // Entering fullscreen closes the menu, which silently drops any open
        // library rename/confirm modal (and the user's typed text with it) —
        // so while one is up, F11 / a header double-click simply waits.
        if !self.fullscreen && (self.lib_rename.is_some() || self.lib_confirm.is_some()) {
            return;
        }
        let on = !self.fullscreen;
        self.set_fullscreen(ctx, on);
    }

    // --- Libraries ---------------------------------------------------------

    /// Snapshot the current panels + layout into a named library. Only PATHS
    /// are captured (never content); a pinned Gallery management is recorded as
    /// the folder it browses.
    fn snapshot_library(&self, name: String) -> crate::config::Library {
        use crate::config::{PanelContent, PanelSnapshot};
        let panels = std::array::from_fn(|i| {
            let content = if self.lm_pinned_at(i).is_some() {
                // A browser pinned HERE snapshots as a Gallery over THIS
                // panel's OWN folder. Its managed target may be a different
                // panel — but that panel's content is captured by its own
                // slot; storing the target's folder here would duplicate it
                // AND silently drop this panel's real (covered) content.
                // (Fidelity note: on apply the browser comes back managing
                // this panel — the cross-panel wiring isn't representable.)
                PanelContent::Gallery { folder: self.panes[i].folder.clone() }
            } else if let (Some(folder), Some(file)) =
                (self.panes[i].folder.clone(), self.panes[i].current_path().cloned())
            {
                PanelContent::Content { folder, file }
            } else {
                PanelContent::Empty
            };
            PanelSnapshot { content, muted: self.muted[i] }
        });
        crate::config::Library {
            name,
            layout: [self.layout.v, self.layout.lh, self.layout.rh],
            panels,
        }
    }

    /// Restore a library: layout, each panel's content (file / empty / pinned
    /// Gallery management on a folder), and the mute states.
    fn apply_library(&mut self, lib: &crate::config::Library) {
        use crate::config::PanelContent;
        // Fresh slate for the pinned-Gallery restores.
        self.list_mgrs.clear();
        self.lm_hidden = false;
        // Lift the presentation cover PROPERLY: cover_thaw resumes whatever the
        // cover froze and clears cover_playing. A bare `frost_all = false` left
        // those flags stale — a later thaw could then spuriously resume media
        // the user had paused, and a restored panel whose content happened to
        // match the old one stayed frozen-paused forever (no reload occurs, so
        // nothing ever reset user_paused).
        self.frost_all = false;
        self.cover_thaw();
        self.layout = Layout::new(lib.layout[0], lib.layout[1], lib.layout[2]);
        for i in 0..4 {
            let snap = &lib.panels[i];
            self.muted[i] = snap.muted;
            // A library restore is a whole fresh workspace: any pane lock from
            // the pre-load session must not leak onto the restored content
            // (locked panes would then invisibly ignore arrows / play-all /
            // clear-all, making the "same" library behave differently per run).
            self.panes[i].locked = false;
            // Missing-content notices are set AFTER the content calls below
            // (after_content_change clears the panel's notice, so the order
            // matters). The library itself is NEVER modified here: reloading
            // it shows the same notices until the user re-writes it by hand.
            let mut missing: Option<LibMissing> = None;
            match &snap.content {
                PanelContent::Empty => {
                    self.panes[i].clear();
                    self.videos[i] = None;
                    self.pdfs[i] = None;
                }
                PanelContent::Content { folder, file } => {
                    if !folder.is_dir() {
                        // The whole folder is gone: the panel stays empty.
                        self.panes[i].clear();
                        self.videos[i] = None;
                        self.pdfs[i] = None;
                        missing = Some(LibMissing {
                            path: folder.clone(),
                            folder_gone: true,
                            pulse_start: None,
                        });
                    } else if !file.is_file() {
                        // Only the file is gone: fall back to the folder's
                        // first file, then business as usual.
                        self.panes[i].set_folder(folder.clone(), None);
                        self.after_content_change(i);
                        missing = Some(LibMissing {
                            path: file.clone(),
                            folder_gone: false,
                            pulse_start: None,
                        });
                    } else {
                        self.panes[i].set_folder(folder.clone(), Some(file));
                        self.after_content_change(i);
                    }
                }
                PanelContent::Gallery { folder } => {
                    match folder {
                        Some(f) if !f.is_dir() => {
                            // The browsed folder is gone: the browser still
                            // comes back, over an empty panel, with a notice.
                            self.panes[i].clear();
                            missing = Some(LibMissing {
                                path: f.clone(),
                                folder_gone: true,
                                pulse_start: None,
                            });
                        }
                        Some(f) => self.panes[i].set_folder(f.clone(), None),
                        None => self.panes[i].clear(),
                    }
                    self.after_content_change(i);
                    self.spawn_lm(Some(i), i); // pinned Gallery management on this panel
                }
            }
            self.lib_missing[i] = missing;
            if let Some(v) = &mut self.videos[i] {
                v.set_muted(self.muted[i]);
            }
        }
    }

    fn library_exists(&self, name: &str) -> bool {
        self.libraries.iter().any(|l| l.name == name)
    }

    /// Save the current state as a NEW library (name must be free + non-empty).
    fn save_library(&mut self, name: &str) {
        if name.is_empty() || self.library_exists(name) {
            return;
        }
        let lib = self.snapshot_library(name.to_string());
        self.libraries.push(lib);
        self.lib_save_error = !crate::config::save_libraries(&self.libraries);
        self.lib_selected = Some(name.to_string());
        self.lib_search.clear();
    }

    /// Overwrite an existing library with the current state.
    fn rewrite_library(&mut self, name: &str) {
        let snap = self.snapshot_library(name.to_string());
        if let Some(l) = self.libraries.iter_mut().find(|l| l.name == name) {
            *l = snap;
            self.lib_save_error = !crate::config::save_libraries(&self.libraries);
        }
    }

    fn rename_library(&mut self, old: &str, new: &str) {
        if new.is_empty() || self.library_exists(new) {
            return;
        }
        if let Some(l) = self.libraries.iter_mut().find(|l| l.name == old) {
            l.name = new.to_string();
            self.lib_save_error = !crate::config::save_libraries(&self.libraries);
            if self.lib_selected.as_deref() == Some(old) {
                self.lib_selected = Some(new.to_string());
            }
        }
    }

    fn delete_library(&mut self, name: &str) {
        self.libraries.retain(|l| l.name != name);
        self.lib_save_error = !crate::config::save_libraries(&self.libraries);
        if self.lib_selected.as_deref() == Some(name) {
            self.lib_selected = None;
        }
    }

    /// Raise / lift the presentation cover (the chrome button and Shift+H).
    /// When "cover freezes content" is on, raising it pauses every panel
    /// (incl. locked); lifting it resumes exactly what was playing.
    fn toggle_cover(&mut self) {
        self.frost_all = !self.frost_all;
        if self.frost_all {
            if self.cover_freezes {
                self.cover_freeze();
            }
        } else {
            self.cover_thaw(); // no-op if the cover didn't freeze anything
        }
    }

    /// Pause every panel, remembering which were actually playing.
    fn cover_freeze(&mut self) {
        for i in 0..4 {
            let mut playing = false;
            if let Some(v) = &mut self.videos[i] {
                if !v.user_paused {
                    playing = true;
                }
                v.user_paused = true;
            }
            if self.panes[i].anim_playing() {
                playing = true;
            }
            self.panes[i].pause_anim();
            self.cover_playing[i] = playing;
        }
    }

    /// Resume the panels the cover paused (leaves everything else as it is).
    fn cover_thaw(&mut self) {
        for i in 0..4 {
            if self.cover_playing[i] {
                if let Some(v) = &mut self.videos[i] {
                    v.user_paused = false;
                }
                self.panes[i].resume_anim();
            }
            self.cover_playing[i] = false;
        }
    }

    /// Empty every unlocked panel (the MulVie-menu Clear button and Shift+C).
    /// Locked panels are left alone, and so are panels covered by a pinned
    /// Gallery management — their frozen content is promised back on unpin
    /// (clearing them would also yank the browser's folder from under it).
    fn clear_all_panels(&mut self) {
        for i in 0..4 {
            if self.panes[i].locked || self.lm_covered(i) {
                continue;
            }
            self.panes[i].clear();
            self.videos[i] = None;
            self.pdfs[i] = None;
            self.lib_missing[i] = None;
        }
    }

    /// Hide/show the divider lines (the header button and the H key).
    fn toggle_dividers(&mut self) {
        self.show_dividers = !self.show_dividers;
        // A manual choice overrides the fullscreen auto-hide, so don't undo
        // it when leaving fullscreen.
        self.dividers_auto_hidden = false;
    }

    /// Single view ↔ MultiView (the header slide switch and the G key); pane
    /// contents are untouched either way.
    fn toggle_view_mode(&mut self, ctx: &Context) {
        let multi = self.layout.pane_rects(self.content_area(ctx)).len() > 1;
        self.layout = if multi {
            Layout::new(1.0, 1.0, 1.0)
        } else {
            Layout::new(0.5, 0.5, 0.5)
        };
    }

    /// When the frosted-glass (acrylic) effect is active the blur carries the
    /// tint, so the fill on top is transparent and lets it show through. Without
    /// acrylic, the chosen colour is drawn solid (its opacity has nothing behind
    /// it to reveal).
    /// The tint colour as straight sRGB (HSVA → Color32 is a clean inverse).
    fn bg_rgb(&self) -> Color32 {
        Color32::from(self.bg_hsva)
    }

    fn canvas_color(&self) -> Color32 {
        if self.glass {
            Color32::TRANSPARENT
        } else {
            self.bg_rgb()
        }
    }

    /// The styling Gallery management borrows from the main window so it looks
    /// like part of the app: the same background fill/glass/tint, and the
    /// user-chosen file-name colour. The LM chrome (titlebar) is unaffected.
    fn lm_style(&self) -> list_manager::LmStyle {
        list_manager::LmStyle {
            glass: self.glass,
            bg_rgb: self.bg_rgb(),
            tint_abgr: self.bg_tint_abgr(),
            text: Color32::from(self.text_hsva),
        }
    }

    /// The background tint as an ABGR value (0xAABBGGRR) for the acrylic API:
    /// the colour's straight RGB, with `bg_alpha` as the opacity.
    fn bg_tint_abgr(&self) -> u32 {
        let [r, g, b, _] = self.bg_rgb().to_array();
        (self.bg_alpha as u32) << 24 | (b as u32) << 16 | (g as u32) << 8 | (r as u32)
    }

    /// Push the current background tint to the acrylic layer when it changes.
    /// Cheap no-op when unchanged or when acrylic isn't active.
    fn apply_bg_tint(&mut self) {
        if !self.glass {
            return;
        }
        let abgr = self.bg_tint_abgr();
        if self.applied_abgr == Some(abgr) {
            return;
        }
        if let Some(hwnd) = self.hwnd {
            crate::os::enable_acrylic(hwnd, abgr);
            self.applied_abgr = Some(abgr);
        }
    }

    /// Try to turn on Windows acrylic once, using the real window handle.
    fn try_enable_glass(&mut self, ctx: &Context, frame: &eframe::Frame) {
        if self.glass_tried {
            return;
        }
        self.glass_tried = true;
        if let Ok(handle) = frame.window_handle() {
            match handle.as_raw() {
                RawWindowHandle::Win32(w) => {
                    let hwnd = w.hwnd.get();
                    self.hwnd = Some(hwnd); // kept for drop-position + focus
                    crate::os::enable_dark_titlebar(hwnd);
                    if crate::os::enable_acrylic(hwnd, self.bg_tint_abgr()) {
                        self.glass = true;
                        self.applied_abgr = Some(self.bg_tint_abgr());
                        ctx.request_repaint();
                    }
                }
                // X11: capture the window id — it drives the drop-position
                // query, the drag-over-main test and the screensaver-suspend
                // scope — and hang the multi-size app icon on the window (the
                // winit-set one has shown a generic gear on Cinnamon). No
                // acrylic on Linux (glass stays false → opaque tint). On
                // Wayland there is no usable global handle: leave None and
                // those features degrade gracefully.
                RawWindowHandle::Xlib(x) => {
                    self.hwnd = Some(x.window as isize);
                    crate::os::set_x11_window_icon(x.window as isize);
                }
                RawWindowHandle::Xcb(x) => {
                    self.hwnd = Some(x.window.get() as isize);
                    crate::os::set_x11_window_icon(x.window.get() as isize);
                }
                _ => {}
            }
        }
    }

    // --- Video -----------------------------------------------------------

    fn pane_is_video(&self, idx: usize) -> bool {
        self.videos[idx].is_some()
            && self.panes[idx]
                .current_path()
                .map(|p| gallery::is_video(p))
                .unwrap_or(false)
    }

    fn pane_is_pdf(&self, idx: usize) -> bool {
        self.pdfium.is_some()
            && self.panes[idx]
                .current_path()
                .map(|p| gallery::is_pdf(p))
                .unwrap_or(false)
    }

    /// True if pane `idx` is backed by the mpv player — a video file OR an
    /// audio-only file. Both render through `show_video_pane` and share the
    /// seek/volume/speed/track machinery.
    fn pane_plays(&self, idx: usize) -> bool {
        self.videos[idx].is_some()
            && self.panes[idx]
                .current_path()
                .map(|p| gallery::is_playable(p))
                .unwrap_or(false)
    }

    /// True if pane `idx` holds an audio-only file — there is no video frame to
    /// zoom or rotate, so those gestures are suppressed for it.
    fn pane_is_audio(&self, idx: usize) -> bool {
        self.panes[idx]
            .current_path()
            .map(|p| gallery::is_audio(p))
            .unwrap_or(false)
    }

    /// The area below the header (matches the central panel).
    fn content_area(&self, ctx: &Context) -> Rect {
        let s = ctx.screen_rect();
        let top = if self.fullscreen { 0.0 } else { HEADER_HEIGHT };
        Rect::from_min_max(pos2(s.left(), s.top() + top), s.max)
    }

    // --- Folder/file dialogs (run off-thread so video keeps playing) ------

    const DLG_OPEN_FILE: &'static str = "Choose a file to open";
    const DLG_OPEN_FOLDER: &'static str = "Choose a folder to open";
    const DLG_MOVE_DEST: &'static str = "Choose where to move the marked files";

    fn start_dialog(&mut self, kind: DialogKind) {
        if let Some(pending) = &self.dialog {
            // One picker at a time. Surface the one already open instead of
            // silently dropping the request (the "does nothing" bug).
            if let Some(hwnd) = crate::os::find_window_by_title(pending.title) {
                crate::os::focus_window(hwnd);
            }
            return;
        }
        // First dialog of the session starts at "This PC" (home on Linux —
        // the CLSID moniker is Windows shell syntax); afterwards at the
        // folder last used in THIS session (never remembered across restarts).
        let start_dir = self.session_dir.lock().ok().and_then(|g| g.clone());
        let start_dir = if cfg!(windows) {
            start_dir.unwrap_or_else(|| PathBuf::from(THIS_PC))
        } else {
            start_dir
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"))
        };
        let (title, pick_folder) = match &kind {
            DialogKind::OpenFile { .. } => (Self::DLG_OPEN_FILE, false),
            DialogKind::OpenFolder { .. } => (Self::DLG_OPEN_FOLDER, true),
            DialogKind::MoveDest { .. } => (Self::DLG_MOVE_DEST, true),
        };
        // Windows only: `ParentWindow` wraps the handle as a Win32 HWND, so
        // it must never be fed the X11 window id captured on Linux (the XDG
        // portal picker there has no raw-handle parenting anyway).
        let parent = if cfg!(windows) { self.hwnd } else { None };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new()
                .set_title(title)
                .set_directory(&start_dir);
            if let Some(h) = parent {
                // Parented pickers always open in FRONT of the main window.
                dlg = dlg.set_parent(&ParentWindow(h));
            }
            let result = if pick_folder {
                dlg.pick_folder()
            } else {
                // Keep this in step with gallery::is_media — every category
                // the app can open must be selectable (rfd's single filter is
                // a hard whitelist with no all-files fallback).
                let mut exts: Vec<&str> = gallery::IMAGE_EXTS.to_vec();
                exts.extend_from_slice(gallery::VIDEO_EXTS);
                exts.extend_from_slice(gallery::AUDIO_EXTS);
                exts.extend_from_slice(gallery::PDF_EXTS);
                dlg.add_filter("Media", &exts).pick_file()
            };
            let _ = tx.send(result);
        });
        self.dialog = Some(PendingDialog { kind, title, rx });
    }

    fn poll_dialog(&mut self, ctx: &Context) {
        let received = match &self.dialog {
            Some(p) => match p.rx.try_recv() {
                Ok(r) => Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint(); // stay alive while the picker is up
                    return;
                }
                Err(_) => {
                    self.dialog = None;
                    return;
                }
            },
            None => return,
        };
        let Some(pending) = self.dialog.take() else {
            return;
        };
        let Some(path) = received.flatten() else { return }; // cancelled
        // Session-only memory: the next dialog starts nearby.
        let dir = if path.is_dir() {
            Some(path.clone())
        } else {
            path.parent().map(|p| p.to_path_buf())
        };
        if let (Some(dir), Ok(mut g)) = (dir, self.session_dir.lock()) {
            *g = Some(dir);
        }
        match pending.kind {
            DialogKind::OpenFile { pane } => {
                if let Some(parent) = path.parent().map(|p| p.to_path_buf()) {
                    self.panes[pane].set_folder(parent, Some(&path));
                    self.after_content_change(pane);
                }
            }
            DialogKind::OpenFolder { pane } => {
                self.panes[pane].set_folder(path, None);
                self.after_content_change(pane);
            }
            DialogKind::MoveDest { files, owner } => {
                if self.move_batch.is_some() {
                    // Belt: a batch is still running (or paused on its
                    // conflict prompt) — never silently discard its queue.
                    self.lm_status(owner, "A move is already in progress.".into());
                } else {
                    let total = files.len();
                    self.move_batch = Some(MoveBatch {
                        dest: path,
                        queue: files.into(),
                        conflict: None,
                        apply_all: None,
                        apply_all_checkbox: false,
                        owner,
                        total,
                        moved: 0,
                        replaced: 0,
                        kept_both: 0,
                        skipped: 0,
                        failed: 0,
                    });
                }
            }
        }
    }

    fn process_dialog_requests(&mut self) {
        for i in 0..4 {
            if let Some(mode) = self.panes[i].dialog_request.take() {
                let DialogMode::File = mode;
                self.start_dialog(DialogKind::OpenFile { pane: i });
            }
        }
    }

    /// After a pane's content was replaced wholesale: tear down viewers of a
    /// mismatched type and reset the pane-local interaction accumulators.
    fn after_content_change(&mut self, idx: usize) {
        let cur = self.panes[idx].current_path().cloned();
        if !cur.as_deref().map(gallery::is_playable).unwrap_or(false) {
            self.videos[idx] = None;
        }
        if !cur.as_deref().map(gallery::is_pdf).unwrap_or(false) {
            self.pdfs[idx] = None;
        }
        self.step_accum[idx] = 0.0;
        self.nav_anim[idx] = None;
        self.video_drag[idx] = None;
        // New content supersedes a "library content missing" notice. (The
        // library-load path sets its notices AFTER calling this, on purpose.)
        self.lib_missing[idx] = None;
    }

    /// Create/load/render the mpv player for any visible pane whose current
    /// file is a video (sized to the pane's pixels for sharpness); pause the
    /// rest.
    fn sync_videos(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        let Some(gl) = frame.gl().cloned() else {
            return;
        };
        let ppp = ctx.pixels_per_point();
        let area = self.content_area(ctx);
        let rects = self.layout.pane_rects(area);

        for i in 0..4 {
            let cur = self.panes[i].current_path().cloned();
            // Video and audio-only files are both driven by the mpv player.
            let plays = cur.as_deref().map(gallery::is_playable).unwrap_or(false);
            // A HIDDEN pane (collapsed divider / Single view) keeps PLAYING —
            // someone may want just the audio — so visibility does not gate
            // this branch; a hidden pane merely renders its frame tiny. Only
            // a panel covered by pinned List Management is frozen: its player
            // pauses, and the user's choice returns on unpin.
            if plays && !self.lm_covered(i) {
                let path = cur.unwrap();
                if self.videos[i].is_none() {
                    self.videos[i] = VideoPlayer::new(&gl, frame, ctx);
                }
                if let Some(vp) = &mut self.videos[i] {
                    if vp.file.as_deref() != Some(path.as_path()) {
                        vp.load_path(&path);
                        vp.set_muted(self.muted[i]);
                        if let Some(paused) = self.load_pause_state[i].take() {
                            // Reload caused by an unpin: restore the play
                            // state captured when the panel was covered.
                            vp.user_paused = paused;
                            vp.ensure_active(true);
                        } else if !self.autoplay {
                            // Autoplay off: every freshly-loaded clip (open or
                            // a nav step) starts paused (load_path defaulted it
                            // to playing).
                            vp.user_paused = true;
                        }
                    } else {
                        self.load_pause_state[i] = None; // no reload was needed
                    }
                    match Self::rect_of(&rects, i) {
                        Some(rect) => {
                            let aspect = vp.aspect().unwrap_or(16.0 / 9.0) as f32;
                            let disp = contain(rect, aspect);
                            // Render the FBO at zoom resolution so zoom stays sharp.
                            let z = vp.zoom;
                            vp.ensure_size(
                                &gl,
                                (disp.width() * ppp * z).round() as i32,
                                (disp.height() * ppp * z).round() as i32,
                            );
                        }
                        // Hidden pane: playback continues, nobody sees the
                        // frame — render at the FBO's minimum size.
                        None => vp.ensure_size(&gl, 16, 16),
                    }
                    vp.ensure_active(true);
                    vp.render_frame(&gl);
                    // An audio clip yields no continuous video frames, so mpv's
                    // update callback stops firing and the only other driver is
                    // the 500ms heartbeat — the seek bar / time readout would
                    // crawl at ~2/sec while playing. Nudge a repaint so the clock
                    // advances at ~4/sec (smooth enough for a 1s readout).
                    if vp.visual_state() != Visual::Frame && !vp.user_paused {
                        ctx.request_repaint_after(std::time::Duration::from_millis(250));
                    }
                }
            } else if let Some(vp) = &mut self.videos[i] {
                vp.ensure_active(false); // pinned-covered / shows an image → pause
            }
        }
    }

    /// Keep Windows awake while media plays: any actively-playing clip stops the
    /// machine sleeping, and a real (moving) video also keeps the display on —
    /// while pure audio lets the screen sleep but keeps the system running so the
    /// music continues. Nothing playing releases the request. Only touches the OS
    /// on a state change (the request is persistent).
    fn update_keep_awake(&mut self) {
        let mut system = false;
        let mut display = false;
        for vp in self.videos.iter().flatten() {
            if vp.is_playing() {
                system = true;
                display |= vp.has_moving_video();
            }
        }
        let want = (system, display);
        if want != self.keep_awake {
            self.keep_awake = want;
            crate::os::set_keep_awake(system, display, self.hwnd);
        }
    }

    fn step_nav(&mut self, idx: usize, notches: f32) {
        self.step_accum[idx] += -notches; // scroll down => next
        while self.step_accum[idx] >= 1.0 {
            self.panes[idx].next();
            self.step_accum[idx] -= 1.0;
        }
        while self.step_accum[idx] <= -1.0 {
            self.panes[idx].prev();
            self.step_accum[idx] += 1.0;
        }
    }

    fn show_video_pane(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect, bg: Color32) {
        let resp = ui.interact(rect, Id::new(("mulvie_vpane", idx)), Sense::click_and_drag());
        // Shown centred for an audio-only file with no cover art (see below).
        let track_name = self.panes[idx]
            .current_path()
            .and_then(|p| p.file_stem().or_else(|| p.file_name()))
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();

        let (tex_id, aspect, progress, user_paused, volume, is_zoomed, zoom, pan, visual) = {
            let vp = self.videos[idx].as_ref().unwrap();
            (
                vp.tex_id,
                vp.aspect().unwrap_or(16.0 / 9.0) as f32,
                vp.progress(),
                vp.user_paused,
                vp.volume,
                vp.is_zoomed(),
                vp.zoom,
                vp.pan,
                vp.visual_state(),
            )
        };

        let disp = contain(rect, aspect);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);
        match visual {
            Visual::Frame => {
                // A real video stream, or an audio file's embedded cover art
                // (mpv renders artwork as a still video frame).
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                // Draw at zoom scale + pan (clipped to the pane by painter_at).
                let vrect = Rect::from_center_size(disp.center() + pan, disp.size() * zoom);
                painter.image(tex_id, vrect, uv, Color32::WHITE);
            }
            Visual::Note => {
                // Audio with no artwork (whatever its container/extension):
                // write the track's name in the centre, kept on screen so you
                // can always see which song is playing.
                if !track_name.is_empty() {
                    let pad = 28.0;
                    let wrap = (rect.width() - 2.0 * pad).max(60.0);
                    let size = (rect.height() * 0.06).clamp(15.0, 30.0);
                    let galley = painter.layout(
                        track_name.clone(),
                        egui::FontId::proportional(size),
                        theme::SILVER,
                        wrap,
                    );
                    let top_left = rect.center() - galley.rect.size() * 0.5;
                    painter.galley(top_left, galley, theme::SILVER);
                }
            }
            // Still parsing headers: plain canvas, so a real video never flashes
            // a placeholder during its brief load window.
            Visual::Loading => {}
        }

        // Hover-control geometry (shared with the fullscreen double-click
        // carve-outs, which must not fire on these bars).
        let (bar, vol, bar_hit, vol_hit) = video_bars(rect);
        let track_bg = Color32::from_rgba_unmultiplied(230, 235, 245, 60);

        // Hide the hover chrome (seek/volume/time) while the cursor is auto-
        // hidden — a hidden pointer means "presentation mode, hands off".
        if resp.hovered() && !self.cursor_hidden {
            // Seek bar.
            painter.rect_filled(bar, egui::Rounding::same(2.5), track_bg);
            if let Some((pos_s, dur_s)) = progress {
                if dur_s > 0.0 {
                    let f = (pos_s / dur_s).clamp(0.0, 1.0) as f32;
                    let fill =
                        Rect::from_min_max(bar.min, pos2(bar.left() + f * bar.width(), bar.bottom()));
                    painter.rect_filled(fill, egui::Rounding::same(2.5), theme::ACCENT);
                }
            }
            // Volume slider (above the pause area).
            painter.rect_filled(vol, egui::Rounding::same(2.5), track_bg);
            let vf = (volume / 130.0) as f32;
            let vfill =
                Rect::from_min_max(vol.min, pos2(vol.left() + vf * vol.width(), vol.bottom()));
            painter.rect_filled(vfill, egui::Rounding::same(2.5), theme::ACCENT);
            painter.circle_filled(
                pos2(vol.left() + vf * vol.width(), vol.center().y),
                5.5,
                Color32::WHITE,
            );
            painter.text(
                pos2(vol.right() + 8.0, vol.center().y),
                egui::Align2::LEFT_CENTER,
                format!("{}%", volume as i32),
                egui::FontId::proportional(11.0),
                theme::SILVER,
            );
            // Time readout, just above the seek bar: "current / total" on the
            // left; when hovering the bar, the jump-to time on the right. If
            // the panel is too narrow for both, only the right one shows; if
            // even that doesn't fit, nothing shows.
            if let Some((pos_s, dur_s)) = progress {
                if dur_s > 0.0 {
                    let font = egui::FontId::proportional(12.0);
                    let left_text = format!("{} / {}", fmt_time(pos_s), fmt_time(dur_s));
                    let right_text = resp
                        .hover_pos()
                        .filter(|p| bar_hit.contains(*p))
                        .map(|p| {
                            let f =
                                ((p.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64;
                            fmt_time(f * dur_s)
                        });
                    let measure = |t: &str| {
                        painter
                            .layout_no_wrap(t.to_owned(), font.clone(), Color32::WHITE)
                            .size()
                            .x
                    };
                    let lw = measure(&left_text);
                    let avail = bar.width();
                    let y = bar.top() - 4.0;
                    let (show_left, show_right) = match &right_text {
                        Some(rt) => {
                            let rw = measure(rt);
                            if lw + rw + 16.0 <= avail {
                                (true, true)
                            } else {
                                (false, rw <= avail)
                            }
                        }
                        None => (lw <= avail, false),
                    };
                    if show_left {
                        shadow_text(
                            &painter,
                            pos2(bar.left() + 1.0, y),
                            egui::Align2::LEFT_BOTTOM,
                            &left_text,
                            font.clone(),
                        );
                    }
                    if show_right {
                        if let Some(rt) = &right_text {
                            shadow_text(
                                &painter,
                                pos2(bar.right() - 1.0, y),
                                egui::Align2::RIGHT_BOTTOM,
                                rt,
                                font.clone(),
                            );
                        }
                    }
                }
            }
            // Pause indicator.
            if user_paused {
                let c = rect.center();
                let col = Color32::from_rgba_unmultiplied(255, 255, 255, 170);
                painter.rect_filled(
                    Rect::from_center_size(pos2(c.x - 8.0, c.y), egui::vec2(6.0, 26.0)),
                    egui::Rounding::same(1.5),
                    col,
                );
                painter.rect_filled(
                    Rect::from_center_size(pos2(c.x + 8.0, c.y), egui::vec2(6.0, 26.0)),
                    egui::Rounding::same(1.5),
                    col,
                );
            }
        }

        // A drag controls ONE thing for its whole lifetime, decided by where
        // it started: on the seek bar => scrubbing, on the volume bar =>
        // volume, elsewhere while zoomed => panning. The bars ignore a pan
        // that merely passes over them (and a scrub keeps scrubbing even if
        // the pointer strays off the bar — standard slider feel).
        if resp.drag_started() {
            self.video_drag[idx] = resp.interact_pointer_pos().and_then(|pos| {
                if bar_hit.contains(pos) {
                    Some(VidDrag::Seek)
                } else if vol_hit.contains(pos) {
                    Some(VidDrag::Volume)
                } else if is_zoomed {
                    Some(VidDrag::Pan)
                } else {
                    None
                }
            });
        }
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| !self.suppress_clicks) {
            let acted = if resp.dragged() {
                match self.video_drag[idx] {
                    Some(VidDrag::Seek) => {
                        let f = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64;
                        if let Some(vp) = &mut self.videos[idx] {
                            vp.seek_fraction(f);
                        }
                        true
                    }
                    Some(VidDrag::Volume) => {
                        let f = ((pos.x - vol.left()) / vol.width()).clamp(0.0, 1.0) as f64;
                        if let Some(vp) = &mut self.videos[idx] {
                            vp.set_volume(f * 130.0);
                        }
                        true
                    }
                    Some(VidDrag::Pan) => {
                        let d = resp.drag_delta();
                        if let Some(vp) = &mut self.videos[idx] {
                            vp.pan_by(disp, d);
                        }
                        true
                    }
                    None => false,
                }
            } else if resp.clicked() && bar_hit.contains(pos) {
                // Plain click on the bar still jumps.
                let f = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64;
                if let Some(vp) = &mut self.videos[idx] {
                    vp.seek_fraction(f);
                }
                true
            } else if resp.clicked() && vol_hit.contains(pos) {
                let f = ((pos.x - vol.left()) / vol.width()).clamp(0.0, 1.0) as f64;
                if let Some(vp) = &mut self.videos[idx] {
                    vp.set_volume(f * 130.0);
                }
                true
            } else {
                false
            };
            if !acted && resp.clicked() {
                // Clicking anywhere on the video (either side or the middle)
                // pauses/resumes. Prev/next is on the right-click menu arrows.
                if let Some(vp) = &mut self.videos[idx] {
                    vp.toggle_pause();
                }
            }
        }
        if resp.drag_stopped() {
            self.video_drag[idx] = None;
        }

        resp.context_menu(|ui| self.video_context_menu(ui, idx));
    }

    fn video_context_menu(&mut self, ui: &mut egui::Ui, idx: usize) {
        ui.set_min_width(widgets::MENU_WIDTH);
        // Nav left, rotate right; rotate keeps the menu open (90°/click).
        let row = widgets::nav_rotate_row(ui, true);
        if row.prev {
            self.panes[idx].prev();
            ui.close_menu();
        }
        if row.next {
            self.panes[idx].next();
            ui.close_menu();
        }
        if row.cw {
            if let Some(v) = &mut self.videos[idx] {
                v.rotate_cw();
            }
        }
        if row.ccw {
            if let Some(v) = &mut self.videos[idx] {
                v.rotate_ccw();
            }
        }
        ui.separator();
        self.panes[idx].folder_menu_items(ui);
        ui.separator();

        ui.menu_button("Custom loop", |ui| {
            for secs in [5.0_f64, 10.0, 30.0] {
                if ui.button(format!("Last {} seconds", secs as i32)).clicked() {
                    if let Some(v) = &mut self.videos[idx] {
                        v.set_ab_loop(secs);
                    }
                    ui.close_menu();
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Last");
                // The default unfocused TextEdit border is invisible on the
                // glassy menu — give the field a visible accent frame.
                ui.scope(|ui| {
                    let w = &mut ui.visuals_mut().widgets;
                    w.inactive.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
                    w.hovered.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
                    ui.add(egui::TextEdit::singleline(&mut self.loop_input).desired_width(42.0));
                });
                ui.label("s");
                if ui.button("Set").clicked() {
                    if let Ok(s) = self.loop_input.trim().parse::<f64>() {
                        if let Some(v) = &mut self.videos[idx] {
                            v.set_ab_loop(s);
                        }
                    }
                    ui.close_menu();
                }
            });
            if ui.button("Clear loop").clicked() {
                if let Some(v) = &mut self.videos[idx] {
                    v.clear_ab_loop();
                }
                ui.close_menu();
            }
        });

        // Playback speed. The A-B loop is defined in media time, so changing
        // speed never shifts where it loops.
        if let Some(cur) = self.videos[idx].as_ref().map(|v| v.speed) {
            let mut chosen = None;
            ui.menu_button("Speed", |ui| {
                chosen = widgets::speed_menu(ui, cur, &mut self.panes[idx].speed_input);
            });
            if let Some(m) = chosen {
                if let Some(v) = &mut self.videos[idx] {
                    v.set_speed(m);
                }
            }
        }

        ui.menu_button("Video adjustments", |ui| {
            let labels = ["Brightness", "Contrast", "Saturation", "Gamma", "Hue"];
            for k in 0..5 {
                let mut val = self.videos[idx].as_ref().map(|v| v.adjust[k]).unwrap_or(0);
                if ui
                    .add(egui::Slider::new(&mut val, -100..=100).text(labels[k]))
                    .changed()
                {
                    if let Some(v) = &mut self.videos[idx] {
                        v.set_adjust(k, val);
                    }
                }
            }
            if ui.button("Reset adjustments").clicked() {
                if let Some(v) = &mut self.videos[idx] {
                    v.reset_adjust();
                }
            }
        });

        // Audio / subtitle track pickers. Read fresh (the clip is loaded by now).
        // Collect the owned lists first so the submenu closures can take &mut.
        let audios = self.videos[idx]
            .as_ref()
            .map(|v| v.tracks("audio"))
            .unwrap_or_default();
        let subs = self.videos[idx]
            .as_ref()
            .map(|v| v.tracks("sub"))
            .unwrap_or_default();

        // Audio: a submenu only when there's a real choice (>1 track).
        if audios.len() >= 2 {
            ui.menu_button("Audio", |ui| {
                for t in &audios {
                    if ui.radio(t.selected, t.label.as_str()).clicked() {
                        if let Some(v) = &mut self.videos[idx] {
                            v.set_audio(t.id);
                        }
                        ui.close_menu();
                    }
                }
            });
        } else {
            ui.add_enabled(false, egui::Button::new("Audio"));
        }

        // Subtitles: "OFF" (default) plus any embedded / external .srt track.
        if subs.is_empty() {
            ui.add_enabled(false, egui::Button::new("Subtitles"));
        } else {
            ui.menu_button("Subtitles", |ui| {
                let any_on = subs.iter().any(|t| t.selected);
                if ui.radio(!any_on, "OFF").clicked() {
                    if let Some(v) = &mut self.videos[idx] {
                        v.set_sub(None);
                    }
                    ui.close_menu();
                }
                for t in &subs {
                    if ui.radio(t.selected, t.label.as_str()).clicked() {
                        if let Some(v) = &mut self.videos[idx] {
                            v.set_sub(Some(t.id));
                        }
                        ui.close_menu();
                    }
                }
            });
        }

        ui.separator();
        if ui.button("Reset zoom").clicked() {
            if let Some(v) = &mut self.videos[idx] {
                v.reset_view();
            }
            ui.close_menu();
        }
        if ui.button("Clear the panel").clicked() {
            self.panes[idx].clear();
            self.videos[idx] = None;
            ui.close_menu();
        }
    }

    fn show_pdf_pane(&mut self, ui: &mut egui::Ui, ctx: &Context, idx: usize, rect: Rect, bg: Color32) {
        let resp = ui.interact(rect, Id::new(("mulvie_pdfpane", idx)), Sense::click_and_drag());
        let Some(path) = self.panes[idx].current_path().cloned() else {
            return;
        };
        let ppp = ctx.pixels_per_point();

        // Ensure a viewer for the current file.
        if self.pdfs[idx].as_ref().map(|v| v.path != path).unwrap_or(true) {
            let count = self
                .pdfium
                .as_ref()
                .and_then(|p| pdf::page_count(p, &path))
                .unwrap_or(0);
            self.pdfs[idx] = Some(PdfView::new(path.clone(), count));
        }

        // Re-render on page / zoom / pane-size change.
        let pane_w = (rect.width() * ppp).max(1.0);
        let pane_h = (rect.height() * ppp).max(1.0);
        let (page, zoom, rotation) = {
            let v = self.pdfs[idx].as_ref().unwrap();
            (v.page, v.zoom, v.rotation)
        };
        let key = (
            page,
            (zoom * 100.0) as i32,
            pane_w as i32 / 32,
            pane_h as i32 / 32,
            rotation,
        );
        if self.pdfs[idx].as_ref().unwrap().rendered != Some(key) {
            let img = self
                .pdfium
                .as_ref()
                .and_then(|p| pdf::render_page(p, &path, page, pane_w, pane_h, zoom, rotation));
            if let Some(img) = img {
                let sz = egui::vec2(img.size[0] as f32 / ppp, img.size[1] as f32 / ppp);
                let tex = ctx.load_texture(format!("mulvie_pdf_{idx}"), img, egui::TextureOptions::LINEAR);
                if let Some(v) = &mut self.pdfs[idx] {
                    v.tex = Some(tex);
                    v.tex_size = sz;
                    v.rendered = Some(key);
                    v.clamp_pan(rect);
                }
            } else if let Some(v) = &mut self.pdfs[idx] {
                v.rendered = Some(key); // avoid retry storm on a bad page
            }
        }

        // Draw.
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);
        let (page, count) = {
            let v = self.pdfs[idx].as_ref().unwrap();
            pdf::draw(&painter, rect, v);
            (v.page, v.page_count)
        };
        if resp.hovered() && count > 0 {
            painter.text(
                pos2(rect.center().x, rect.bottom() - 14.0),
                egui::Align2::CENTER_CENTER,
                format!("{} / {}", page + 1, count),
                egui::FontId::proportional(12.0),
                theme::SILVER,
            );
        }

        // Drag-pan when zoomed; side click = prev/next file.
        let is_zoomed = self.pdfs[idx].as_ref().map(|v| v.zoom > 1.0001).unwrap_or(false);
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| !self.suppress_clicks) {
            if is_zoomed && resp.dragged() {
                let d = resp.drag_delta();
                if let Some(v) = &mut self.pdfs[idx] {
                    v.drag_pan(rect, d);
                }
            } else if resp.clicked() {
                let x = (pos.x - rect.left()) / rect.width();
                if x < IMG_SIDE {
                    self.panes[idx].prev();
                } else if x > 1.0 - IMG_SIDE {
                    self.panes[idx].next();
                }
            }
        }

        resp.context_menu(|ui| self.pdf_context_menu(ui, idx));
    }

    fn pdf_context_menu(&mut self, ui: &mut egui::Ui, idx: usize) {
        ui.set_min_width(widgets::MENU_WIDTH);
        // Nav left, rotate right; rotate keeps the menu open (90°/click).
        let row = widgets::nav_rotate_row(ui, true);
        if row.prev {
            self.panes[idx].prev();
            ui.close_menu();
        }
        if row.next {
            self.panes[idx].next();
            ui.close_menu();
        }
        if row.cw {
            if let Some(v) = &mut self.pdfs[idx] {
                v.rotate_cw();
            }
        }
        if row.ccw {
            if let Some(v) = &mut self.pdfs[idx] {
                v.rotate_ccw();
            }
        }
        ui.separator();
        self.panes[idx].folder_menu_items(ui);
        ui.separator();

        let count = self.pdfs[idx].as_ref().map(|v| v.page_count).unwrap_or(0);
        if count > 0 {
            ui.menu_button("Jump to page", |ui| {
                let mut page1 = self.pdfs[idx].as_ref().map(|v| v.page + 1).unwrap_or(1) as i32;
                if ui
                    .add(egui::Slider::new(&mut page1, 1..=count as i32).text("page"))
                    .changed()
                {
                    if let Some(v) = &mut self.pdfs[idx] {
                        v.set_page((page1 - 1).max(0) as u16);
                    }
                }
            });
            ui.separator();
        }
        if ui.button("Reset zoom").clicked() {
            if let Some(v) = &mut self.pdfs[idx] {
                v.reset_zoom();
            }
            ui.close_menu();
        }
        if ui.button("Clear the panel").clicked() {
            self.panes[idx].clear();
            self.pdfs[idx] = None;
            ui.close_menu();
        }
    }

    // --- Input routing ---------------------------------------------------

    fn handle_keys(&mut self, ctx: &Context) {
        let (f11, esc) = ctx.input(|i| (i.key_pressed(Key::F11), i.key_pressed(Key::Escape)));
        if f11 {
            self.toggle_fullscreen(ctx);
        }
        if esc {
            // Modal layers own Escape, innermost first; fullscreen exit is
            // only ever the LAST resort (cancelling a prompt must not also
            // throw the presentation out of fullscreen).
            if self.delete_confirm.is_some() {
                self.delete_confirm = None;
            } else if self.lm_delete_confirm.is_some() {
                self.lm_delete_confirm = None;
            } else if self
                .move_batch
                .as_ref()
                .map(|b| b.conflict.is_some())
                .unwrap_or(false)
            {
                // Same as the prompt's "Cancel the move".
                if let Some(b) = &mut self.move_batch {
                    b.queue.clear();
                    b.conflict = None;
                    b.skipped += 1;
                }
            } else if !self.lm_hidden
                && self
                    .list_mgrs
                    .iter()
                    .any(|m| m.pinned.is_some() && m.modal_open())
            {
                // A pinned browser's rename modal renders in THIS window.
                for m in &mut self.list_mgrs {
                    if m.pinned.is_some() && m.modal_open() {
                        m.close_modal();
                        break;
                    }
                }
            } else if self.fullscreen {
                self.set_fullscreen(ctx, false);
            }
        }
    }

    /// Double-click anywhere in fullscreen returns to a normal window — except
    /// where double-clicking would fight an existing gesture: the prev/next
    /// side strips (rapid click-browsing must not throw the presentation out
    /// of fullscreen), the video seek/volume bars, and a pinned List
    /// Management (double-click there means "show this file").
    fn handle_fullscreen_dblclick(&mut self, ctx: &Context, rects: &[(Quadrant, Rect)]) {
        if !self.fullscreen
            || self.suppress_clicks
            || self.bg_menu_open
            || self.modal_open()
            || self.lm_drag_active()
        {
            return;
        }
        if egui::menu::BarState::load(ctx, Id::new("__egui::context_menu")).is_some() {
            return;
        }
        if !ctx.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary)) {
            return;
        }
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let Some(idx) = Self::pane_at(rects, pos) else {
            return;
        };
        if self.lm_covered(idx) {
            return;
        }
        let Some(rect) = Self::rect_of(rects, idx) else {
            return;
        };
        let has_content = self.panes[idx].current_path().is_some();
        if dblclick_exits_fullscreen(pos, rect, self.pane_plays(idx), has_content) {
            self.set_fullscreen(ctx, false);
        }
    }

    /// Auto-hide the cursor when it rests over a content-bearing pane for 4s
    /// without moving (or clicking). Sets `self.cursor_hidden` for the frame;
    /// the actual `CursorIcon::None` is applied at the very end of `update` so
    /// nothing drawn later overrides it. Schedules a repaint so the hide fires
    /// even while the app is otherwise idle. No-op unless the toggle is on.
    fn update_cursor_hide(&mut self, ctx: &Context, rects: &[(Quadrant, Rect)]) {
        self.cursor_hidden = false;
        // Only auto-hide in a presentation posture — fullscreen or maximized.
        // In a normal windowed session the cursor always stays visible.
        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        if !self.mouse_hide || !(self.fullscreen || maximized) {
            self.last_pointer_pos = None;
            return;
        }
        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
            self.last_pointer_pos = None;
            return;
        };
        let now = ctx.input(|i| i.time);
        let moved = self.last_pointer_pos.map_or(true, |p| p.distance(pos) > 0.5);
        // Any active interaction counts as activity, not just raw pointer
        // motion: the wheel drives browse/zoom/volume and there are mouse-over
        // key shortcuts, so those must keep the cursor up even if it sits still.
        let active = ctx.input(|i| {
            i.pointer.any_pressed()
                || i.raw_scroll_delta != egui::Vec2::ZERO
                || i.smooth_scroll_delta != egui::Vec2::ZERO
                || !i.keys_down.is_empty()
        });
        if moved || active {
            self.last_pointer_move = now;
        }
        self.last_pointer_pos = Some(pos);

        // Never hide while a context menu or the delete-confirm dialog is open —
        // the user needs the pointer to aim at those, even after a pause to read.
        let popup_open =
            egui::menu::BarState::load(ctx, Id::new("__egui::context_menu")).is_some();
        if popup_open || self.modal_open() {
            self.last_pointer_move = now; // so it doesn't instantly hide on close
            return;
        }

        // Only hide over a pane that actually has content (not empty panes,
        // the header/chrome, or a pinned List Management — that's interactive).
        let over_content = Self::pane_at(rects, pos).map_or(false, |idx| {
            !self.lm_covered(idx) && self.panes[idx].current_path().is_some()
        });
        if !over_content {
            return;
        }
        const HIDE_AFTER: f64 = 4.0;
        let idle = now - self.last_pointer_move;
        if idle >= HIDE_AFTER {
            self.cursor_hidden = true;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(HIDE_AFTER - idle));
        }
    }

    /// Mouse-over keyboard shortcuts for the pane under the cursor:
    /// Space = pause/resume video or GIF, R / Shift+R = rotate cw/ccw,
    /// F / Shift+F = next/previous file, Delete = confirm-then-recycle the
    /// current file.
    fn handle_shortcuts(&mut self, ctx: &Context, rects: &[(Quadrant, Rect)]) {
        // Never fire while typing (rename fields, custom-loop input, …), while a
        // context menu / the MulVie menu is open (their own R/Space/Delete would
        // act on the pane behind), or while the delete-confirm dialog is up.
        let menu_open =
            egui::menu::BarState::load(ctx, Id::new("__egui::context_menu")).is_some();
        if ctx.wants_keyboard_input() || menu_open || self.bg_menu_open || self.modal_open() {
            return;
        }
        // Window-global keys (not tied to a hovered pane), gated above so they
        // never fire while typing in a search/rename field:
        //   H  = divider lines     Shift+H = frost all panels
        //   G  = the 1 ↔ 4 layout  Shift+C = clear all panels
        //   ←/→ = step EVERY panel prev/next at once (locked panes excluded)
        let (key_h, key_g, key_c, key_left, key_right, mods) = ctx.input(|i| {
            (
                i.key_pressed(Key::H),
                i.key_pressed(Key::G),
                i.key_pressed(Key::C),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.modifiers,
            )
        });
        let plain = !mods.any();
        let shift_only = mods.shift && !mods.ctrl && !mods.alt && !mods.command;
        if key_h && plain {
            self.toggle_dividers();
        }
        if key_h && shift_only {
            self.toggle_cover();
        }
        // While the presentation cover is up, every OTHER shortcut is inert:
        // the panes are invisible, but their rects still hit-test, so without
        // this gate keys would act on unseen content — Shift+C clearing
        // everything, arrows/G stepping or relaying it out, and Delete (below)
        // opening a confirm THAT NAMES THE HIDDEN FILE above the cover.
        // Only H (dividers) and Shift+H (lift the cover) stay live.
        if self.frost_all {
            return;
        }
        if key_g && plain {
            self.toggle_view_mode(ctx);
        }
        if key_c && shift_only {
            self.clear_all_panels();
        }
        if (key_left || key_right) && plain {
            // Advance/retreat all panels together; with one panel this is just
            // that panel, matching F / Shift+F. Locked panes hold their place,
            // and so does content frozen under a pinned Gallery management
            // (it must return on unpin exactly as it was covered).
            for i in 0..4 {
                if self.panes[i].locked || self.lm_covered(i) {
                    continue;
                }
                if key_right {
                    self.panes[i].next();
                } else {
                    self.panes[i].prev();
                }
            }
        }
        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        let Some(idx) = Self::pane_at(rects, pos) else {
            return;
        };
        if self.lm_covered(idx) {
            return; // List Management occupies this panel
        }
        let (space, r, f, shift, del) = ctx.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::R),
                i.key_pressed(Key::F),
                i.modifiers.shift,
                i.key_pressed(Key::Delete),
            )
        });
        if space {
            if self.pane_plays(idx) {
                if let Some(v) = &mut self.videos[idx] {
                    v.toggle_pause();
                }
            } else {
                self.panes[idx].toggle_anim_pause();
            }
        }
        if r {
            if self.pane_is_video(idx) {
                if let Some(v) = &mut self.videos[idx] {
                    if shift {
                        v.rotate_ccw();
                    } else {
                        v.rotate_cw();
                    }
                }
            } else if self.pane_is_pdf(idx) {
                if let Some(v) = &mut self.pdfs[idx] {
                    if shift {
                        v.rotate_ccw();
                    } else {
                        v.rotate_cw();
                    }
                }
            } else if !self.pane_is_audio(idx) {
                // Audio-only panes have nothing to rotate; images do.
                self.panes[idx].rotate(if shift { 3 } else { 1 });
            }
        }
        if f {
            if shift {
                self.panes[idx].prev();
            } else {
                self.panes[idx].next();
            }
        }
        if del && self.delete_confirm.is_none() {
            if let Some(p) = self.panes[idx].current_path().cloned() {
                self.delete_confirm = Some((p, pos));
            }
        }
    }

    /// The "Are you sure?" prompt opened by the Delete key, at the cursor, in
    /// the same right-click-menu style (not an app window) for consistency.
    fn show_delete_confirm(&mut self, ctx: &Context) {
        let Some((path, at)) = self.delete_confirm.clone() else {
            return;
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("this file")
            .to_owned();
        let mut yes = false;
        let mut cancel = false;

        // Full-screen catcher behind the popup: a click anywhere outside just
        // dismisses the prompt and is swallowed, so it doesn't also act on a
        // pane — exactly how clicking away from a context menu behaves.
        let screen = ctx.screen_rect();
        let catcher = egui::Area::new(Id::new("mulvie_del_catcher"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .constrain(false)
            .show(ctx, |ui| ui.allocate_rect(screen, Sense::click()));
        let clicked_outside = catcher.inner.clicked();

        // The prompt itself, drawn with the menu frame at the cursor (above the
        // catcher). `constrain` keeps it on-screen near an edge.
        egui::Area::new(Id::new("mulvie_del_popup"))
            .order(egui::Order::Tooltip)
            .fixed_pos(at)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_max_width(240.0);
                    ui.label(
                        RichText::new("Delete this file?")
                            .color(theme::INK_BLUE)
                            .strong(),
                    );
                    ui.label(RichText::new(&name).color(theme::INK_BLUE).size(11.0));
                    ui.separator();
                    if ui.button("Yes, delete").clicked() {
                        yes = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if yes {
            self.delete_confirm = None;
            self.delete_file(&path);
        } else if cancel || clicked_outside {
            self.delete_confirm = None;
        }
    }

    /// Delete `path` (recycle bin first, permanent as fallback) and drop it
    /// from every pane's browse list. ANY pane's video/pdf player that is
    /// holding this exact file is torn down first — keyed on the player's own
    /// file, not on what a pane currently shows — so Windows releases the
    /// handle and the move succeeds.
    fn delete_file(&mut self, path: &Path) {
        self.release_holders(path); // Drop stops mpv + releases the file
        if delete_from_disk(path) {
            for p in &mut self.panes {
                p.remove_file(path);
            }
            // Every browser drops the dead path from its selection (a ghost
            // entry would keep inflating the "N selected" count), and the
            // caches free its pixels — a later file reusing the name must
            // not inherit them.
            for m in &mut self.list_mgrs {
                m.unselect(path);
            }
            self.store.invalidate(path);
            self.thumbs.invalidate(path);
        }
    }

    /// Menu-confirmed deletes ("Delete file → Are you sure → Yes") — deletes the
    /// path captured when the delete was armed, not the pane's live file.
    fn process_delete_requests(&mut self) {
        for i in 0..4 {
            if let Some(p) = self.panes[i].delete_request.take() {
                self.delete_file(&p);
            }
        }
    }

    /// "Switch panel content" menu picks. A panel occupied by pinned List
    /// Management never takes part (the menu disables it; this is the belt).
    fn process_switch_requests(&mut self) {
        for i in 0..4 {
            if let Some(t) = self.panes[i].switch_request.take() {
                if t < 4
                    && t != i
                    && self.lm_pinned_at(t).is_none()
                    && self.lm_pinned_at(i).is_none()
                {
                    self.swap_panels(i, t);
                }
            }
        }
    }

    // --- List Management: multi-instance rules -----------------------------

    /// The instance MANAGING `panel`, if any (one panel, one manager).
    fn lm_managing(&self, panel: usize) -> Option<usize> {
        self.list_mgrs.iter().position(|m| m.target == panel)
    }

    /// The instance PINNED INTO `panel`, if any.
    fn lm_pinned_at(&self, panel: usize) -> Option<usize> {
        self.list_mgrs.iter().position(|m| m.pinned == Some(panel))
    }

    /// True when a VISIBLE pinned browser covers this panel (its content is
    /// frozen and it takes no input or new content). Hidden browsers do not
    /// cover — the panel then behaves normally, content paused.
    fn lm_covered(&self, panel: usize) -> bool {
        !self.lm_hidden && self.lm_pinned_at(panel).is_some()
    }

    /// The first panel (A,B,C,D order) no instance manages yet.
    fn lm_free_panel(&self) -> Option<usize> {
        [0usize, 2, 1, 3]
            .into_iter()
            .find(|p| self.lm_managing(*p).is_none())
    }

    /// True while any browser's selection is being dragged toward a panel.
    fn lm_drag_active(&self) -> bool {
        self.list_mgrs.iter().any(|m| m.drag.is_some())
    }

    /// True while an app-modal prompt is up (delete keys, wheel and the
    /// fullscreen gesture must not reach the panes underneath).
    fn modal_open(&self) -> bool {
        self.delete_confirm.is_some()
            || self.lm_delete_confirm.is_some()
            || self
                .move_batch
                .as_ref()
                .map(|b| b.conflict.is_some())
                .unwrap_or(false)
    }

    /// Freeze what `panel` is playing (a pin is about to cover it); returns
    /// whether it was actually playing, so unpin can restore exactly that.
    fn freeze_panel(&mut self, idx: usize) -> bool {
        let mut playing = false;
        if let Some(v) = &mut self.videos[idx] {
            if !v.user_paused && self.panes[idx].current_path().is_some() {
                playing = true;
            }
            v.user_paused = true;
        }
        if self.panes[idx].anim_playing() {
            playing = true;
        }
        self.panes[idx].pause_anim();
        playing
    }

    /// Release `panel` on unpin: content that was PLAYING when the pin
    /// covered it resumes; paused content stays paused — including when the
    /// current file CHANGED while covered (the reload applies the captured
    /// state).
    fn release_panel(&mut self, idx: usize, was_playing: bool) {
        let cur = self.panes[idx].current_path().cloned();
        let playable = cur.as_deref().map(gallery::is_playable).unwrap_or(false);
        let loaded = self.videos[idx].as_ref().and_then(|v| v.file.clone());
        if playable {
            if cur == loaded {
                if let Some(v) = &mut self.videos[idx] {
                    v.user_paused = !was_playing;
                }
            } else {
                self.load_pause_state[idx] = Some(!was_playing);
            }
        } else if was_playing {
            self.panes[idx].resume_anim();
        }
    }

    /// Spawn a browser instance. The caller enforces the instance cap and the
    /// one-manager-per-panel rule.
    fn spawn_lm(&mut self, pinned: Option<usize>, target: usize) {
        let used: std::collections::HashSet<usize> =
            self.list_mgrs.iter().map(|m| m.slot).collect();
        let slot = (0..list_manager::MAX_INSTANCES)
            .find(|s| !used.contains(s))
            .unwrap_or(0);
        let mut lm = ListManager::new(slot, pinned, target);
        if let Some(idx) = pinned {
            lm.covered_was_playing = self.freeze_panel(idx);
        }
        self.list_mgrs.push(lm);
    }

    /// Hide or show every browser at once (the chrome toggle). Hiding lifts
    /// the pinned covers — those panels show their frozen content, paused,
    /// and behave normally; the windows disappear. All state is kept.
    /// Showing covers the pinned panels again, re-capturing their play state.
    fn set_lm_hidden(&mut self, hidden: bool) {
        if self.lm_hidden == hidden {
            return;
        }
        self.lm_hidden = hidden;
        if !hidden {
            let pins: Vec<(usize, usize)> = self
                .list_mgrs
                .iter()
                .enumerate()
                .filter_map(|(k, m)| m.pinned.map(|p| (k, p)))
                .collect();
            for (k, p) in pins {
                // Re-freezing can only UPGRADE the captured state: content the
                // pin left paused reads as not-playing here even though it was
                // playing when originally covered — overwriting with that
                // would make a hide/show round-trip lose the resume-on-unpin.
                // (If the user resumed it while the browsers were hidden, the
                // fresh capture is true and wins.)
                let playing = self.freeze_panel(p);
                self.list_mgrs[k].covered_was_playing |= playing;
            }
            // The standalone windows are recreated: acrylic must re-apply.
            for m in &mut self.list_mgrs {
                m.reset_glass();
            }
        }
    }

    /// Close one instance for real (its X): restore its pinned panel, drop it.
    fn close_lm_instance(&mut self, k: usize) {
        // Closing mid-duplicates-view counts as leaving it: the pane gets its
        // pre-duplicates marks back (the wipe must not outlive the browser).
        self.list_mgrs[k].restore_marks(&mut self.panes);
        let pinned = self.list_mgrs[k].pinned;
        let was = self.list_mgrs[k].covered_was_playing;
        if let Some(idx) = pinned {
            if !self.lm_hidden {
                self.release_panel(idx, was);
            }
            // Hidden: the panel is already uncovered (content stays paused).
        }
        self.list_mgrs.remove(k);
    }

    /// Act on the requests raised by the pane right-click menus and each
    /// browser's own controls (pin/unpin, close, pickers, file operations).
    fn process_lm_requests(&mut self, ctx: &Context) {
        // "List management" in a pane's right-click menu: a NEW browser pinned
        // to that panel, managing it — or the first unmanaged panel when this
        // one is already managed elsewhere. Four browsers is the ceiling.
        for i in 0..4 {
            if !self.panes[i].list_manage_request {
                continue;
            }
            self.panes[i].list_manage_request = false;
            if self.lm_pinned_at(i).is_some() {
                // A (hidden) browser is already pinned right here: reveal all.
                self.set_lm_hidden(false);
                continue;
            }
            if self.list_mgrs.len() >= list_manager::MAX_INSTANCES {
                // No fifth browser: glow the chrome icon red instead.
                self.lm_flash = Some(ctx.input(|inp| inp.time));
                continue;
            }
            let target = if self.lm_managing(i).is_none() {
                i
            } else {
                self.lm_free_panel().unwrap_or(i)
            };
            self.set_lm_hidden(false);
            self.spawn_lm(Some(i), target);
        }

        // Per-instance wishes.
        let mut close: Vec<usize> = Vec::new();
        for k in 0..self.list_mgrs.len() {
            if let Some(req) = self.list_mgrs[k].pin_request.take() {
                match req {
                    Some(idx) if self.list_mgrs[k].pinned != Some(idx) => {
                        if self.lm_pinned_at(idx).is_some() {
                            // Taken by another instance (the UI dims these).
                        } else {
                            if let Some(old) = self.list_mgrs[k].pinned {
                                let was = self.list_mgrs[k].covered_was_playing;
                                if !self.lm_hidden {
                                    self.release_panel(old, was);
                                }
                            }
                            if !self.lm_hidden {
                                let playing = self.freeze_panel(idx);
                                self.list_mgrs[k].covered_was_playing = playing;
                            }
                            self.list_mgrs[k].pinned = Some(idx);
                            // The standalone window is destroyed while pinned;
                            // the one that returns on unpin is a NEW window.
                            self.list_mgrs[k].reset_glass();
                        }
                    }
                    Some(_) => {} // pin to where it already is: nothing to do
                    None => {
                        if let Some(old) = self.list_mgrs[k].pinned {
                            let was = self.list_mgrs[k].covered_was_playing;
                            if !self.lm_hidden {
                                self.release_panel(old, was);
                            }
                        }
                        self.list_mgrs[k].pinned = None;
                        self.list_mgrs[k].reset_glass();
                    }
                }
            }
            if self.list_mgrs[k].open_folder_request {
                self.list_mgrs[k].open_folder_request = false;
                let pane = self.list_mgrs[k].target;
                self.start_dialog(DialogKind::OpenFolder { pane });
            }
            if let Some(paths) = self.list_mgrs[k].dropped_paths.take() {
                // Files/folders dropped onto the standalone browser window:
                // they replace the managed panel's content.
                let target = self.list_mgrs[k].target;
                self.drop_paths_into_pane(target, paths);
            }
            if let Some(files) = self.list_mgrs[k].delete_request.take() {
                let slot = self.list_mgrs[k].slot;
                if self.lm_delete_confirm.is_none() && self.lm_delete_queue.is_none() {
                    self.lm_delete_confirm = Some((files, slot));
                    // The prompt renders in the MAIN window; the request may
                    // come from a standalone browser covering it — raise it.
                    self.focus_main(ctx);
                } else {
                    self.lm_status(slot, "A delete is already in progress.".into());
                }
            }
            if let Some(files) = self.list_mgrs[k].move_request.take() {
                let slot = self.list_mgrs[k].slot;
                if self.move_batch.is_some() {
                    // A batch is running or paused on its conflict prompt:
                    // starting another would discard its remaining queue.
                    self.lm_status(slot, "A move is already in progress.".into());
                    self.focus_main(ctx);
                } else {
                    self.start_dialog(DialogKind::MoveDest { files, owner: slot });
                }
            }
            if let Some(plan) = self.list_mgrs[k].rename_request.take() {
                self.apply_lm_rename(k, plan);
            }
            if self.list_mgrs[k].close_request {
                self.list_mgrs[k].close_request = false;
                close.push(k);
            }
        }
        for k in close.into_iter().rev() {
            self.close_lm_instance(k);
        }
    }

    /// Refresh every pane whose browse tree overlaps `root` after files under
    /// it changed on disk: the pane browsing `root` itself, one browsing a
    /// SUBFOLDER of it (the changed files may sit right there), and one
    /// browsing an ANCESTOR (its recursive tree contains `root`).
    fn refresh_panes_under(&mut self, root: &Path) {
        for i in 0..4 {
            if let Some(f) = self.panes[i].folder.clone() {
                if f.starts_with(root) || root.starts_with(&f) {
                    self.panes[i].refresh_folder();
                }
            }
        }
    }

    /// Apply a browser's confirmed rename batch: tear down any player holding
    /// a source open first (Windows refuses to rename an open file), then
    /// refresh every overlapping pane and drop cached pixels for the touched
    /// paths (a batch can reuse a name freed within itself — a cache keyed on
    /// that path would keep showing the OLD file's image).
    fn apply_lm_rename(&mut self, k: usize, plan: Vec<(PathBuf, PathBuf)>) {
        let touched: Vec<PathBuf> = plan
            .iter()
            .flat_map(|(old, new)| [old.clone(), new.clone()])
            .collect();
        for (old, _) in &plan {
            self.release_holders(old);
        }
        let outcome = file_ops::apply_renames(plan);
        self.list_mgrs[k].rename_done = Some(outcome.summary());
        for p in &touched {
            self.store.invalidate(p);
            self.thumbs.invalidate(p);
        }
        if let Some(root) = self.panes[self.list_mgrs[k].target].folder.clone() {
            self.refresh_panes_under(&root);
        }
        self.prune_dead_selections();
    }

    /// Drop dead paths from every browser's selection after a batch changed
    /// the disk — the surviving selection is kept (a batch in one browser
    /// must not wipe another browser's unrelated selection).
    fn prune_dead_selections(&mut self) {
        for m in &mut self.list_mgrs {
            m.prune_dead_selection();
        }
    }

    /// Put a message in the counts row of the browser with this SLOT (slots
    /// are stable across closes, unlike Vec indices). Gone browser = no-op.
    fn lm_status(&mut self, slot: usize, msg: String) {
        if let Some(m) = self.list_mgrs.iter_mut().find(|m| m.slot == slot) {
            m.status = msg;
        }
    }

    /// Bring the main window forward (its in-app prompts must not sit unseen
    /// behind a standalone browser window).
    fn focus_main(&self, ctx: &Context) {
        // Windows: raw-handle focus (restores a minimized window explicitly).
        // Elsewhere the viewport command does the same through winit — its X11
        // focus_window also un-minimizes first.
        if cfg!(windows) {
            if let Some(h) = self.hwnd {
                crate::os::focus_window(h);
                return;
            }
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
    }

    /// Tear down any player/viewer holding `path` open (Windows won't move or
    /// delete a file with an open handle).
    fn release_holders(&mut self, path: &Path) {
        for j in 0..4 {
            if self.videos[j].as_ref().and_then(|v| v.file.as_deref()) == Some(path) {
                self.videos[j] = None;
            }
            if self.pdfs[j].as_ref().map(|v| v.path.as_path()) == Some(path) {
                self.pdfs[j] = None;
            }
        }
    }

    // --- List Management: delete-marked + move-marked -----------------------

    /// The "Move N marked files to the Recycle Bin?" confirmation.
    fn show_lm_delete_confirm(&mut self, ctx: &Context) {
        let Some((files, owner)) = self.lm_delete_confirm.clone() else {
            return;
        };
        let mut yes = false;
        let mut cancel = false;
        let screen = ctx.screen_rect();
        let catcher = egui::Area::new(Id::new("mulvie_lmdel_catcher"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .constrain(false)
            .show(ctx, |ui| ui.allocate_rect(screen, Sense::click()));
        egui::Area::new(Id::new("mulvie_lmdel_popup"))
            .order(egui::Order::Tooltip)
            .fixed_pos(screen.center() - egui::vec2(140.0, 60.0))
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_max_width(280.0);
                    let msg = if files.len() == 1 {
                        // A one-file batch (the per-file menu, or a single
                        // marked file): name it — clearer than a count.
                        format!(
                            "Move \"{}\" to the {TRASH_NAME}?",
                            file_ops::name_of(&files[0])
                        )
                    } else {
                        format!("Move {} marked file(s) to the {TRASH_NAME}?", files.len())
                    };
                    ui.label(RichText::new(msg).color(theme::INK_BLUE).strong());
                    ui.separator();
                    if ui.button("Yes, delete").clicked() {
                        yes = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if yes {
            self.lm_delete_confirm = None;
            let total = files.len();
            self.lm_delete_queue = Some((files.into(), total, owner));
        } else if cancel || catcher.inner.clicked() {
            self.lm_delete_confirm = None;
        }
    }

    /// Drain the confirmed delete batch one file per frame: each recycle-bin
    /// move is a shell call that can take tens of milliseconds, so a big
    /// batch done in one frame would freeze the window ("Not Responding").
    fn process_lm_delete(&mut self, ctx: &Context) {
        let Some((mut queue, total, owner)) = self.lm_delete_queue.take() else {
            return;
        };
        if let Some(p) = queue.pop_front() {
            self.delete_file(&p);
        }
        if queue.is_empty() {
            self.prune_dead_selections();
            self.lm_status(owner, format!("Deleted {total} file(s) to the {TRASH_NAME}."));
        } else {
            let k = total - queue.len();
            self.lm_status(owner, format!("Deleting {k} of {total}…"));
            self.lm_delete_queue = Some((queue, total, owner));
            ctx.request_repaint();
        }
    }

    /// Work the move batch: ONE file per frame (a big batch, or one slow
    /// cross-volume copy per file, must keep the window alive and show its
    /// progress) until a name conflict needs the user or the queue drains.
    fn process_move_batch(&mut self, ctx: &Context) {
        let Some(mut batch) = self.move_batch.take() else {
            return;
        };
        if batch.conflict.is_none() {
            if let Some(src) = batch.queue.pop_front() {
                let name = file_ops::name_of(&src);
                if !src.exists() {
                    batch.skipped += 1;
                } else if name.is_empty() {
                    // A name that can't be represented (non-Unicode) would
                    // alias the DESTINATION FOLDER itself — the "conflict"
                    // prompt's Replace would then recycle the whole folder.
                    batch.failed += 1;
                } else {
                    let dst = batch.dest.join(&name);
                    if dst == src {
                        batch.skipped += 1;
                    } else if dst.exists() {
                        match batch.apply_all {
                            Some(choice) => self.apply_move_choice(&mut batch, &src, choice),
                            None => {
                                batch.conflict = Some(src);
                                // The prompt renders in the MAIN window; the
                                // batch may have been started from a
                                // standalone browser covering it — raise it.
                                self.focus_main(ctx);
                            }
                        }
                    } else if self.move_one(&src, &dst) {
                        batch.moved += 1;
                    } else {
                        batch.failed += 1;
                    }
                }
            }
        }

        if batch.conflict.is_none() && batch.queue.is_empty() {
            // Done: summarize, refresh every pane (source root AND any pane
            // whose tree contains the destination).
            let mut msg = format!("Moved {} file(s).", batch.moved + batch.kept_both + batch.replaced);
            if batch.kept_both > 0 {
                msg.push_str(&format!(" {} kept both (renamed).", batch.kept_both));
            }
            if batch.replaced > 0 {
                msg.push_str(&format!(" {} replaced.", batch.replaced));
            }
            if batch.skipped > 0 {
                msg.push_str(&format!(" {} skipped.", batch.skipped));
            }
            if batch.failed > 0 {
                msg.push_str(&format!(" {} failed.", batch.failed));
            }
            for i in 0..4 {
                if self.panes[i].folder.is_some() {
                    self.panes[i].refresh_folder();
                }
            }
            self.prune_dead_selections();
            self.lm_status(batch.owner, msg);
        } else {
            if batch.conflict.is_none() {
                let k = batch.total - batch.queue.len();
                self.lm_status(batch.owner, format!("Moving {k} of {}…", batch.total));
                // Keep frames coming while the queue drains; a batch paused
                // on its prompt just waits (no repaint storm).
                ctx.request_repaint();
            }
            self.move_batch = Some(batch);
        }
    }

    /// The Windows-style conflict prompt: Cancel / Replace / Keep both, with
    /// "do this for all conflicts", plus a small size/date comparison.
    fn show_move_conflict(&mut self, ctx: &Context) {
        let Some(batch) = &self.move_batch else { return };
        let Some(src) = batch.conflict.clone() else {
            return;
        };
        let dst = batch.dest.join(file_ops::name_of(&src));
        let mut choice: Option<ConflictChoice> = None;
        let mut cancel = false;
        let mut apply_all = batch.apply_all_checkbox;
        let describe = |p: &Path| {
            std::fs::metadata(p)
                .map(|m| {
                    let mb = m.len() as f64 / (1024.0 * 1024.0);
                    let when = m
                        .modified()
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map(|e| format!("{:.0} day(s) old", e.as_secs_f64() / 86_400.0))
                        .unwrap_or_default();
                    format!("{mb:.2} MB, {when}")
                })
                .unwrap_or_else(|_| "unreadable".into())
        };

        let screen = ctx.screen_rect();
        egui::Area::new(Id::new("mulvie_move_catcher"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .constrain(false)
            .show(ctx, |ui| ui.allocate_rect(screen, Sense::click()));
        egui::Area::new(Id::new("mulvie_move_popup"))
            .order(egui::Order::Tooltip)
            .fixed_pos(screen.center() - egui::vec2(180.0, 90.0))
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.label(
                        RichText::new(format!(
                            "\"{}\" already exists in the destination.",
                            file_ops::name_of(&src)
                        ))
                        .color(theme::INK_BLUE)
                        .strong(),
                    );
                    ui.label(
                        RichText::new(format!("Existing:  {}", describe(&dst)))
                            .color(theme::INK_BLUE)
                            .size(11.0),
                    );
                    ui.label(
                        RichText::new(format!("Moving:    {}", describe(&src)))
                            .color(theme::INK_BLUE)
                            .size(11.0),
                    );
                    ui.separator();
                    if ui.button("Replace the existing file").clicked() {
                        choice = Some(ConflictChoice::Replace);
                    }
                    if ui.button("Keep both files").clicked() {
                        choice = Some(ConflictChoice::KeepBoth);
                    }
                    if ui.button("Skip this file").clicked() {
                        choice = Some(ConflictChoice::Skip);
                    }
                    ui.separator();
                    ui.checkbox(
                        &mut apply_all,
                        RichText::new("Do this for all conflicts").color(theme::INK_BLUE),
                    );
                    if ui.button("Cancel the move").clicked() {
                        cancel = true;
                    }
                });
            });

        let Some(batch) = &mut self.move_batch else {
            return;
        };
        batch.apply_all_checkbox = apply_all;
        if cancel {
            batch.queue.clear();
            batch.conflict = None;
            batch.skipped += 1;
        } else if let Some(c) = choice {
            batch.conflict = None;
            if apply_all {
                batch.apply_all = Some(c);
            }
            let mut batch = self.move_batch.take().unwrap();
            self.apply_move_choice(&mut batch, &src, c);
            self.move_batch = Some(batch);
        }
    }

    fn apply_move_choice(&mut self, batch: &mut MoveBatch, src: &Path, choice: ConflictChoice) {
        let name = file_ops::name_of(src);
        let dst = batch.dest.join(&name);
        match choice {
            ConflictChoice::Skip => batch.skipped += 1,
            ConflictChoice::Replace => {
                // The displaced file goes to the Recycle Bin (recoverable) —
                // but only once its replacement has SAFELY arrived. The
                // fallible, possibly expensive part (the source may be locked
                // by another program; a cross-volume copy may fail) happens
                // first, under a temp name in the destination folder, so a
                // failed move can never leave the destination already
                // destroyed.
                if dst.is_dir() {
                    // A FOLDER with the file's name: never recycle that.
                    batch.failed += 1;
                    return;
                }
                let tmp = {
                    let mut counter = 0usize;
                    loop {
                        let cand = batch.dest.join(format!(".mulvie_mvtmp_{counter}"));
                        counter += 1;
                        if !cand.exists() {
                            break cand;
                        }
                    }
                };
                if !self.move_one(src, &tmp) {
                    batch.failed += 1;
                    return;
                }
                self.release_holders(&dst);
                if trash::delete(&dst).is_ok() || !dst.exists() {
                    if std::fs::rename(&tmp, &dst).is_ok() {
                        // The path now holds DIFFERENT content: drop any
                        // cached full-size image or thumbnail keyed on it.
                        self.store.invalidate(&dst);
                        self.thumbs.invalidate(&dst);
                        batch.replaced += 1;
                    } else {
                        // A same-folder rename after a successful recycle
                        // barely can fail; keep the data under a visible name.
                        let alt = file_ops::keep_both_name(&batch.dest, &name);
                        let _ = std::fs::rename(&tmp, &alt);
                        batch.kept_both += 1;
                    }
                } else {
                    // The recycle was refused: put the source back where it
                    // came from; failing even that, keep it visible in the
                    // destination under a "(n)" name rather than a temp name.
                    if file_ops::move_file(&tmp, src).is_ok() {
                        batch.failed += 1;
                    } else {
                        let alt = file_ops::keep_both_name(&batch.dest, &name);
                        let _ = std::fs::rename(&tmp, &alt);
                        batch.kept_both += 1;
                    }
                }
            }
            ConflictChoice::KeepBoth => {
                let alt = file_ops::keep_both_name(&batch.dest, &name);
                if self.move_one(src, &alt) {
                    batch.kept_both += 1;
                } else {
                    batch.failed += 1;
                }
            }
        }
    }

    /// Move one file, releasing any player holding it first (with the same
    /// brief retry the delete path uses for the handle-release lag).
    fn move_one(&mut self, src: &Path, dst: &Path) -> bool {
        self.release_holders(src);
        for _ in 0..10 {
            if file_ops::move_file(src, dst).is_ok() {
                for p in &mut self.panes {
                    p.remove_file(src);
                }
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        false
    }

    /// While a List-Management selection drag is live: highlight the panel
    /// under the REAL cursor (the drag usually comes from a separate window,
    /// whose pointer capture the main window can't see); on release over a
    /// panel, that panel adopts the dragged subset.
    fn process_lm_drag(&mut self, ctx: &Context) {
        let Some(src) = self.list_mgrs.iter().position(|m| m.drag.is_some()) else {
            return;
        };
        // Resolve the panel under the global cursor — but only when the cursor
        // is truly over the MAIN window: a browser window may overlap it, and
        // a release over the browser must not fall through to a pane.
        let ppp = ctx.pixels_per_point();
        let over_main = self
            .hwnd
            .map_or(false, |h| crate::os::cursor_over_window(h));
        let target = if over_main {
            self.hwnd
                .and_then(crate::os::cursor_pos_in_client)
                .map(|(x, y)| pos2(x / ppp, y / ppp))
                .and_then(|pos| {
                    let rects = self.layout.pane_rects(self.content_area(ctx));
                    Self::pane_at(&rects, pos)
                        .filter(|i| !self.lm_covered(*i))
                        .map(|i| (i, Self::rect_of(&rects, i).unwrap_or(Rect::NOTHING)))
                })
        } else {
            None
        };

        let dropped = self.list_mgrs[src]
            .drag
            .as_ref()
            .map(|d| d.dropped)
            .unwrap_or(false);
        if dropped {
            let payload = self.list_mgrs[src].drag.take();
            if let (Some((idx, _)), Some(payload)) = (target, payload) {
                let keep: std::collections::HashSet<PathBuf> =
                    payload.picked.iter().cloned().collect();
                self.panes[idx].adopt_selection(payload.folder, payload.files, &keep);
                self.after_content_change(idx);
            }
            ctx.request_repaint();
            return;
        }

        // Drag still in flight: light up the panel it would land in.
        if let Some((_, rect)) = target {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                Id::new("lm_drop_hint"),
            ));
            painter.rect_stroke(
                rect.shrink(2.0),
                egui::Rounding::same(4.0),
                Stroke::new(3.0_f32, theme::ACCENT),
            );
        }
        // Keep the main window repainting while the pointer is captured by a
        // browser window, so the highlight follows the cursor.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }

    /// Swap the WHOLE content of two panels (folder, current file, viewers).
    /// The audio (mute) setting belongs to the panel position — the numbered
    /// header buttons keep meaning the same panel — so it's re-applied to
    /// whichever player arrives.
    fn swap_panels(&mut self, a: usize, b: usize) {
        self.panes.swap(a, b);
        self.panes[a].index = a;
        self.panes[b].index = b;
        self.videos.swap(a, b);
        self.pdfs.swap(a, b);
        self.lib_missing.swap(a, b); // the notice follows its content
        // A picker in flight is bound to a pane index; keep it pointing at
        // the content it was opened for. (delete_confirm is path-based, so it
        // needs no fix-up.)
        if let Some(pending) = &mut self.dialog {
            let di = match &mut pending.kind {
                DialogKind::OpenFile { pane } | DialogKind::OpenFolder { pane } => Some(pane),
                DialogKind::MoveDest { .. } => None,
            };
            if let Some(di) = di {
                if *di == a {
                    *di = b;
                } else if *di == b {
                    *di = a;
                }
            }
        }
        self.load_pause_state.swap(a, b);
        for &i in &[a, b] {
            if let Some(v) = &mut self.videos[i] {
                v.set_muted(self.muted[i]);
            }
            self.step_accum[i] = 0.0;
            self.nav_anim[i] = None;
            self.video_drag[i] = None;
        }
    }

    /// Frameless-window resize: thin drag zones along the window edges and
    /// corners that ask the OS to begin a resize. No-op while maximized or
    /// fullscreen (matching how Windows disables edge-resize then).
    fn window_resize(&self, ctx: &Context) {
        if self.fullscreen || ctx.input(|i| i.viewport().maximized).unwrap_or(false) {
            return;
        }
        for (name, zone, dir, cursor) in resize_zones(ctx.screen_rect(), RESIZE_BAND) {
            place_resize_area(ctx, "main", name, zone, dir, cursor);
        }
    }

    fn pane_at(rects: &[(Quadrant, Rect)], pos: egui::Pos2) -> Option<usize> {
        rects
            .iter()
            .find(|(_, r)| r.contains(pos))
            .map(|(q, _)| *q as usize)
    }

    fn rect_of(rects: &[(Quadrant, Rect)], idx: usize) -> Option<Rect> {
        rects
            .iter()
            .find(|(q, _)| *q as usize == idx)
            .map(|(_, r)| *r)
    }

    fn handle_scroll(&mut self, ctx: &Context, rects: &[(Quadrant, Rect)]) {
        // Don't let the wheel drive the pane behind an open overlay (the MulVie
        // menu — where scrolling over the sliders is natural — or any modal
        // prompt), NOR invisible panes behind the presentation cover (their
        // rects still hit-test; the wheel would browse/zoom/volume unseen
        // content). The panes are resolved purely by cursor position, so
        // they'd otherwise scroll underneath the popup.
        if self.bg_menu_open || self.modal_open() || self.frost_all {
            return;
        }
        let (events, hover) = ctx.input(|i| (i.events.clone(), i.pointer.hover_pos()));
        for ev in events {
            let egui::Event::MouseWheel {
                unit,
                delta,
                modifiers,
            } = ev
            else {
                continue;
            };
            // Normalise to "notches".
            let notches = match unit {
                egui::MouseWheelUnit::Line => delta.y,
                egui::MouseWheelUnit::Point => delta.y / POINT_PER_STEP,
                egui::MouseWheelUnit::Page => delta.y,
            };
            if notches == 0.0 {
                continue;
            }

            if modifiers.ctrl {
                if let Some(pos) = hover {
                    if let Some(idx) = Self::pane_at(rects, pos) {
                        if self.lm_covered(idx) {
                            continue; // List Management owns this panel's input
                        }
                        if let Some(rect) = Self::rect_of(rects, idx) {
                            if self.pane_is_pdf(idx) {
                                let factor = 1.1_f32.powf(notches);
                                if let Some(v) = &mut self.pdfs[idx] {
                                    v.zoom_at(rect, pos, factor);
                                }
                            } else if self.pane_is_video(idx) {
                                if let Some(vp) = &mut self.videos[idx] {
                                    let aspect = vp.aspect().unwrap_or(16.0 / 9.0) as f32;
                                    let disp = contain(rect, aspect);
                                    let factor = 1.1_f32.powf(notches);
                                    vp.zoom_at(disp, pos, factor);
                                }
                            } else if !self.pane_is_audio(idx) {
                                // Audio-only panes have no frame to zoom into.
                                let factor = 1.1_f32.powf(notches);
                                self.panes[idx].zoom_at(rect, pos, factor);
                            }
                        }
                    }
                }
            } else if modifiers.alt {
                // All (visible) panes step together — but video panes ignore
                // alt+scroll (their scroll is reserved for volume), and locked
                // panes hold their place like every other all-panes command.
                self.alt_accum += -notches;
                let visible: Vec<usize> = rects
                    .iter()
                    .map(|(q, _)| *q as usize)
                    .filter(|&i| {
                        !self.pane_plays(i) && !self.lm_covered(i) && !self.panes[i].locked
                    })
                    .collect();
                while self.alt_accum >= 1.0 {
                    for &i in &visible {
                        self.panes[i].next();
                    }
                    self.alt_accum -= 1.0;
                }
                while self.alt_accum <= -1.0 {
                    for &i in &visible {
                        self.panes[i].prev();
                    }
                    self.alt_accum += 1.0;
                }
            } else if let Some(pos) = hover {
                if let Some(idx) = Self::pane_at(rects, pos) {
                    if self.lm_covered(idx) {
                        continue; // the browser's own scroll area handles this
                    }
                    let rect = Self::rect_of(rects, idx).unwrap_or(Rect::NOTHING);
                    if self.pane_is_pdf(idx) {
                        // Scroll always navigates PDF pages (never file, never zoom).
                        self.step_accum[idx] += -notches;
                        while self.step_accum[idx] >= 1.0 {
                            if let Some(v) = &mut self.pdfs[idx] {
                                v.next_page();
                            }
                            self.step_accum[idx] -= 1.0;
                        }
                        while self.step_accum[idx] <= -1.0 {
                            if let Some(v) = &mut self.pdfs[idx] {
                                v.prev_page();
                            }
                            self.step_accum[idx] += 1.0;
                        }
                    } else if self.pane_plays(idx) {
                        // Plain scroll over a video/audio pane deliberately does
                        // nothing (ctrl+scroll zooms video; volume is the slider).
                    } else if self.panes[idx].zoom > 1.0001 {
                        // Zoomed image: scroll pans vertically.
                        self.panes[idx].pan_scroll(rect, notches * PAN_STEP);
                    } else {
                        self.step_nav(idx, notches);
                    }
                }
            }
        }
    }

    /// Open `path` in pane `idx` exactly like "Open file" did: the file shows,
    /// its folder (whole subtree) becomes the browse list, whatever was there
    /// is replaced. Shared by drag-and-drop, the open-with inbox, and startup
    /// arguments.
    fn open_file_in_pane(&mut self, idx: usize, path: &Path) {
        let Some(parent) = path.parent() else { return };
        self.panes[idx].set_folder(parent.to_path_buf(), Some(path));
        self.after_content_change(idx);
    }

    /// Drop a supported file onto a pane: behaves exactly like "Open file",
    /// individually per pane. Unsupported files are ignored.
    ///
    /// The pane is chosen by the REAL cursor position at the drop moment,
    /// asked from Windows directly — winit discards the drop coordinates, so
    /// egui's pointer state is stale during an OS drag (it froze wherever the
    /// mouse last was before the drag), which is why drops used to land only
    /// in the top-left pane.
    fn handle_file_drop(&mut self, ctx: &Context, rects: &[(Quadrant, Rect)]) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let ppp = ctx.pixels_per_point();
        let pos = self
            .hwnd
            .and_then(crate::os::cursor_pos_in_client)
            .map(|(x, y)| pos2(x / ppp, y / ppp))
            .or_else(|| {
                ctx.input(|i| {
                    i.pointer
                        .interact_pos()
                        .or_else(|| i.pointer.hover_pos())
                        .or_else(|| i.pointer.latest_pos())
                })
            });
        let Some(pos) = pos else { return };
        let Some(idx) = Self::pane_at(rects, pos) else {
            return;
        };
        let paths: Vec<PathBuf> = dropped.iter().filter_map(|f| f.path.clone()).collect();
        if let Some(k) = if self.lm_hidden { None } else { self.lm_pinned_at(idx) } {
            // Dropping ONTO a pinned Gallery management replaces the content
            // of the panel it MANAGES.
            let target = self.list_mgrs[k].target;
            self.drop_paths_into_pane(target, paths);
            return;
        }
        self.drop_paths_into_pane(idx, paths);
    }

    /// Load the first supported dropped item into pane `idx`: a folder opens
    /// its whole subtree (first supported file shown), a file opens like
    /// "Open file". Shared by pane drops, drops onto a pinned browser, and
    /// drops onto a standalone browser window.
    fn drop_paths_into_pane(&mut self, idx: usize, paths: Vec<PathBuf>) {
        for path in paths {
            if path.is_dir() {
                let files = gallery::scan_folder(&path, self.panes[idx].sort);
                if !files.is_empty() {
                    self.panes[idx].set_scanned(path, files, None);
                    self.after_content_change(idx);
                    return;
                }
            } else if gallery::is_media(&path) {
                self.open_file_in_pane(idx, &path);
                return;
            }
        }
    }

    /// Open-with hand-over: a second MulVie launched with a file writes it to
    /// the inbox file and exits; the running instance picks it up here and
    /// shows it in the top-left panel, then comes to the foreground.
    fn poll_inbox(&mut self, ctx: &Context) {
        let Some(inbox) = crate::config::inbox_path() else {
            return;
        };
        // Cheap existence probe every frame; the file exists only for the
        // moment of a hand-over.
        if !inbox.exists() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&inbox) else {
            return;
        };
        let _ = std::fs::remove_file(&inbox);
        let path = PathBuf::from(text.trim());
        if path.is_file() && gallery::is_media(&path) {
            // Placement: single view -> that panel; multi view -> the first
            // unoccupied visible panel in A,B,C,D order; all occupied -> the
            // first visible one. A panel holding pinned List Management is
            // never a target; if that leaves nowhere visible, the hand-over
            // is ignored (rare: single view showing only the pinned browser).
            let visible: Vec<usize> = self
                .layout
                .pane_rects(self.content_area(ctx))
                .iter()
                .map(|(q, _)| *q as usize)
                .filter(|i| !self.lm_covered(*i))
                .collect();
            let target = if visible.len() <= 1 {
                visible.first().copied()
            } else {
                [0usize, 2, 1, 3] // A=TL, B=TR, C=BL, D=BR
                    .into_iter()
                    .find(|i| visible.contains(i) && self.panes[*i].current_path().is_none())
                    .or_else(|| visible.first().copied())
            };
            let Some(target) = target else { return };
            self.open_file_in_pane(target, &path);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if let Some(hwnd) = self.hwnd {
                crate::os::focus_window(hwnd);
            }
            ctx.request_repaint();
        }
    }

    // --- Dividers --------------------------------------------------------

    fn dividers(&mut self, ui: &mut egui::Ui, area: Rect) {
        // Holding TAB does two things: it temporarily REVEALS the divider lines
        // if they're hidden, and it pulls any divider stuck against a window
        // edge (where the resize band / header eats its grab zone) a few pixels
        // inward so it can be caught. A line the user never drags snaps back the
        // moment TAB is released; a dragged line keeps its new spot. The divider
        // the user is CURRENTLY dragging is left alone (`held`), so dragging one
        // toward the edge isn't fought by the nudge. (This used to be Alt, but
        // on Linux Alt+left-drag is the window manager's whole-window move —
        // it stole exactly the grab this feature exists for.)
        let reveal = ui.input(|i| i.key_down(Key::Tab));
        let show = self.show_dividers || reveal;
        let nudge = |real: f32, lo: f32, hi: f32, held: bool| -> f32 {
            if !reveal || held {
                real
            } else if real < lo + REVEAL_NEAR_EDGE {
                lo + REVEAL_NUDGE
            } else if real > hi - REVEAL_NEAR_EDGE {
                hi - REVEAL_NUDGE
            } else {
                real
            }
        };

        let x = area.left() + self.layout.v * area.width();
        let disp_x = nudge(x, area.left(), area.right(), self.div_dragging[0]);

        // Vertical divider — full height, always present (grab at edge to
        // re-expand a collapsed column).
        {
            let hit = Rect::from_min_max(
                pos2(disp_x - DIVIDER_GRAB * 0.5, area.top()),
                pos2(disp_x + DIVIDER_GRAB * 0.5, area.bottom()),
            );
            let r = ui.interact(hit, Id::new("mulvie_div_v"), Sense::drag());
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if r.dragged() {
                // First drag frame of a nudged line: commit it to the drawn
                // (nudged) position so the drag continues from there.
                if disp_x != x {
                    self.layout.v = ((disp_x - area.left()) / area.width().max(1.0)).clamp(0.0, 1.0);
                }
                self.layout.v =
                    (self.layout.v + r.drag_delta().x / area.width().max(1.0)).clamp(0.0, 1.0);
            }
            self.div_dragging[0] = r.dragged();
            if show {
                let col = divider_color(&r);
                ui.painter()
                    .vline(disp_x, area.top()..=area.bottom(), Stroke::new(DIVIDER_THICK, col));
            }
        }

        // Left column horizontal divider.
        if self.layout.left_visible() {
            let ly = area.top() + self.layout.lh * area.height();
            let disp_y = nudge(ly, area.top(), area.bottom(), self.div_dragging[1]);
            let hit = Rect::from_min_max(
                pos2(area.left(), disp_y - DIVIDER_GRAB * 0.5),
                pos2(disp_x, disp_y + DIVIDER_GRAB * 0.5),
            );
            let r = ui.interact(hit, Id::new("mulvie_div_lh"), Sense::drag());
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            }
            if r.dragged() {
                if disp_y != ly {
                    self.layout.lh =
                        ((disp_y - area.top()) / area.height().max(1.0)).clamp(0.0, 1.0);
                }
                self.layout.lh =
                    (self.layout.lh + r.drag_delta().y / area.height().max(1.0)).clamp(0.0, 1.0);
            }
            self.div_dragging[1] = r.dragged();
            if show {
                let col = divider_color(&r);
                ui.painter()
                    .hline(area.left()..=disp_x, disp_y, Stroke::new(DIVIDER_THICK, col));
            }
        } else {
            self.div_dragging[1] = false;
        }

        // Right column horizontal divider.
        if self.layout.right_visible() {
            let ry = area.top() + self.layout.rh * area.height();
            let disp_y = nudge(ry, area.top(), area.bottom(), self.div_dragging[2]);
            let hit = Rect::from_min_max(
                pos2(disp_x, disp_y - DIVIDER_GRAB * 0.5),
                pos2(area.right(), disp_y + DIVIDER_GRAB * 0.5),
            );
            let r = ui.interact(hit, Id::new("mulvie_div_rh"), Sense::drag());
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            }
            if r.dragged() {
                if disp_y != ry {
                    self.layout.rh =
                        ((disp_y - area.top()) / area.height().max(1.0)).clamp(0.0, 1.0);
                }
                self.layout.rh =
                    (self.layout.rh + r.drag_delta().y / area.height().max(1.0)).clamp(0.0, 1.0);
            }
            self.div_dragging[2] = r.dragged();
            if show {
                let col = divider_color(&r);
                ui.painter()
                    .hline(disp_x..=area.right(), disp_y, Stroke::new(DIVIDER_THICK, col));
            }
        } else {
            self.div_dragging[2] = false;
        }
    }

    // --- Header ----------------------------------------------------------

    /// The "MulVie" dropdown: the relocated toggles plus the background-tint
    /// controls. Opens under the wordmark; click-away or Esc closes it. Styled
    /// like the app's own windows (panel fill + thin border) so it reads as part
    /// of MulVie. Live-previews and persists the tint via the normal config save.
    fn show_bg_menu(&mut self, ctx: &Context) {
        if !self.bg_menu_open {
            // The library modals only live while the menu is open; drop them.
            self.lib_rename = None;
            self.lib_confirm = None;
            self.menu_kb_focus_prev = false;
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            // Escape peels ONE layer at a time: a library-row context menu
            // first (egui closes it itself but does NOT consume the Esc — see
            // menu.rs — so without this check the whole menu would close too),
            // then typing focus in the name field (egui already dropped the
            // focus while PROCESSING the Esc, which is why last frame's state
            // is the only reliable gate), then an open rename/confirm modal,
            // and only then the menu itself.
            let ctx_menu_open =
                egui::menu::BarState::load(ctx, Id::new("__egui::context_menu")).is_some();
            if ctx_menu_open || self.menu_kb_focus_prev {
                // egui handles those layers; the menu stays.
            } else if self.lib_rename.is_some() || self.lib_confirm.is_some() {
                self.lib_rename = None;
                self.lib_confirm = None;
            } else {
                self.bg_menu_open = false;
            }
            self.menu_kb_focus_prev = ctx.wants_keyboard_input();
            return;
        }

        // Click-away catcher — over the CONTENT area only, NOT the header, so the
        // header (and the wordmark) stay draggable/clickable while the menu is
        // open: a click on a pane closes the menu (and is swallowed), while the
        // wordmark toggle and window-drag still work. Opening the menu clicks the
        // wordmark, which is outside this catcher, so it can't self-close.
        //
        // Order::MIDDLE (a level BELOW the Foreground menu), NOT Foreground:
        // egui sorts layers by `order` as the PRIMARY key, so a Middle catcher
        // can NEVER stack above the Foreground menu — the invariant "menu above
        // its own catcher" is structural, not draw-order-dependent. Two same-
        // Order Areas can otherwise flip: on a relayout frame (e.g. loading a
        // library changes the layout, hence content_area) the catcher would grab
        // `wants_to_be_on_top` and sort above the menu, then swallow every menu
        // click — and while a library modal blocked the close-gate, the menu
        // would appear frozen open with dead clicks. Middle removes that class.
        // Middle still sits above the Background panes, so it keeps catching pane
        // clicks and closing the menu on an outside click.
        let content = self.content_area(ctx);
        let catcher = egui::Area::new(Id::new("mulvie_menu_catcher"))
            .order(egui::Order::Middle)
            .fixed_pos(content.min)
            .constrain(false)
            .show(ctx, |ui| ui.allocate_rect(content, Sense::click()));
        let clicked_away = catcher.inner.clicked();

        egui::Area::new(Id::new("mulvie_menu"))
            // Foreground (not Tooltip) so egui's context menus — which render at
            // Foreground — appear ON TOP of the menu, not behind it. It sits above
            // the click-away catcher structurally now (catcher is Order::Middle).
            .order(egui::Order::Foreground)
            .fixed_pos(self.menu_anchor)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0_f32, theme::ACCENT_DIM))
                    .rounding(egui::Rounding::same(5.0))
                    .inner_margin(egui::Margin::same(9.0))
                    .show(ui, |ui| {
                        // PIN the width (min == max) so the Settings and Library
                        // panels are always the SAME width — neither can be pushed
                        // wider by its own content (the Library name field used to
                        // widen the menu). The width includes a symmetric gutter
                        // on both sides: content (and the rules) stays 258 and
                        // CENTRED, and the floating scrollbar rides in the right
                        // gutter with clearance from everything instead of
                        // overlapping right-aligned buttons.
                        const MENU_GUTTER: f32 = 14.0;
                        ui.set_width(258.0 + 2.0 * MENU_GUTTER);

                        // Top row: Settings + Library (which each expand a panel
                        // below) and — pushed to the far right so it's hard to
                        // hit by accident — Clear all. (Loop folder lives in the
                        // Startup & playback row of the Settings section.)
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.add_space(MENU_GUTTER); // align with the inset sections
                            if widgets::icon_button(
                                ui,
                                Icon::Settings,
                                self.bg_menu_colors,
                                "Settings — startup, playback and colours",
                            )
                            .clicked()
                            {
                                self.bg_menu_colors = !self.bg_menu_colors;
                                if self.bg_menu_colors {
                                    self.bg_menu_library = false;
                                    self.menu_scroll_reset = true; // fresh section: start at the top
                                }
                            }
                            if widgets::icon_button(
                                ui,
                                Icon::Library,
                                self.bg_menu_library,
                                "Library — save & load panel layouts",
                            )
                            .clicked()
                            {
                                self.bg_menu_library = !self.bg_menu_library;
                                if self.bg_menu_library {
                                    self.bg_menu_colors = false;
                                    self.menu_scroll_reset = true; // fresh section: start at the top
                                }
                            }
                            // Clear all — right-aligned, separated from the rest.
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.add_space(MENU_GUTTER); // mirror the left inset
                                if widgets::icon_button(ui, Icon::Clear, false, "Clear all panels  (Shift+C)")
                                    .clicked()
                                {
                                    self.clear_all_panels();
                                }
                            });
                        });

                        // Settings or Library panel (mutually exclusive), only
                        // when its icon is toggled on. Settings' height is the
                        // reference; the Library panel pads its list to match, so
                        // the two menus are the SAME size regardless of content.
                        //
                        // A short window can't fit the whole section — cap it to
                        // the space left above the window's bottom edge and let it
                        // scroll. The scrollbar is FLOATING: drawn OVER the content
                        // (at most covering the rules' right ends) and allocating
                        // zero width, so it never shifts or resizes the layout by
                        // appearing/disappearing.
                        if self.bg_menu_colors || self.bg_menu_library {
                            let max_h = (ui.ctx().screen_rect().bottom()
                                - ui.next_widget_position().y
                                - 14.0) // frame margin + border + a small gap
                                .max(60.0);
                            // Shrinking the WINDOW with the menu open displaces
                            // the area: constrain() clamps with LAST frame's
                            // size, and max_h above re-derives from the clamped
                            // position — a feedback loop that reseats the menu
                            // only ~5px per rendered frame. Keep frames flowing
                            // while displaced from the anchor so the crawl
                            // finishes in ~100ms instead of riding the 500ms
                            // heartbeat for seconds.
                            let area_top = ui.max_rect().top() - 9.0; // undo the frame margin
                            if (area_top - self.menu_anchor.y).abs() > 0.5 {
                                ui.ctx().request_repaint();
                            }
                            // Keep the idle handle faintly visible (floating bars
                            // default to fully invisible until hovered) — without
                            // it nothing says "there is more below".
                            ui.spacing_mut().scroll.dormant_handle_opacity = 0.5;
                            let mut scroll = egui::ScrollArea::vertical();
                            if self.menu_scroll_reset {
                                self.menu_scroll_reset = false;
                                scroll = scroll.vertical_scroll_offset(0.0);
                            }
                            scroll
                                // Per-section scroll STATE: with a shared id,
                                // switching sections would carry the other's
                                // offset over — a bottom-scrolled Settings
                                // would open Library showing only its
                                // pad-to-height slack (i.e. nothing).
                                .id_salt(("mulvie_menu_sections", self.bg_menu_colors))
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                                )
                                .max_height(max_h)
                                // min == max, deliberately: inside an auto-sized
                                // Area the ui's "available height" is last
                                // frame's area size, which would cap the
                                // viewport at the 64px scroll-minimum and then
                                // FREEZE it there (the area can only remember
                                // what the capped content produced). The
                                // explicit floor overrides that; auto_shrink
                                // still collapses shorter content.
                                .min_scrolled_height(max_h)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    // Symmetric insets: content keeps its 258px
                                    // width centred in the wider menu, and the
                                    // scrollbar never touches it or the rules.
                                    let inset =
                                        egui::Margin::symmetric(MENU_GUTTER, 0.0);
                                    egui::Frame::none().inner_margin(inset).show(ui, |ui| {
                                    if self.bg_menu_colors {
                                        let y0 = ui.min_rect().bottom();
                                        self.bg_menu_color_section(ui);
                                        self.menu_section_h = ui.min_rect().bottom() - y0;
                                    } else {
                                        // Fall back to a measured constant until
                                        // Settings has been shown once this session
                                        // to record its height. (534 = Settings
                                        // incl. the About row; re-measure when its
                                        // content changes, or Library-first pads
                                        // short and the menu pops on first switch.)
                                        let target = if self.menu_section_h > 1.0 {
                                            self.menu_section_h
                                        } else {
                                            534.0
                                        };
                                        self.bg_menu_library_section(ui, target);
                                    }
                                    });
                                });
                        }
                    });
            });

        // Library rename / confirm prompts, on top of the menu. While one is
        // up, the menu must not close underneath it.
        let modal_active = self.lib_modals(ctx);

        // Close the menu on ANY click outside it — including on the chrome.
        // The wordmark is excluded (it toggles the menu itself); its click is
        // handled in `header`, which runs first and closes the menu already,
        // so a click reaching here over the header is a non-wordmark chrome
        // click and should dismiss the menu.
        let header_click = ctx.input(|i| {
            i.pointer.primary_clicked()
                && i.pointer
                    .interact_pos()
                    .map(|p| p.y < HEADER_HEIGHT && !self.brand_rect.contains(p))
                    .unwrap_or(false)
        });
        if !modal_active && (clicked_away || header_click) {
            self.bg_menu_open = false;
        }

        // Record whether something in the menu (the library name field) holds
        // keyboard focus, for next frame's Esc layering — egui clears the
        // focus while PROCESSING an Esc press, so the live value is already
        // false by the time the handler above runs.
        self.menu_kb_focus_prev = ctx.wants_keyboard_input();
    }

    /// The MulVie menu's expandable settings section: the persistent
    /// startup/playback toggles, then the background-tint and item-name-text
    /// colour palettes. Shown only when the settings icon is toggled on.
    fn bg_menu_color_section(&mut self, ui: &mut egui::Ui) {
        let ds = Stroke::new(1.0_f32, theme::ACCENT_DIM);
        let double_rule = |ui: &mut egui::Ui| {
            let w = ui.available_width();
            let (sr, _) = ui.allocate_exact_size(egui::vec2(w, 9.0), Sense::hover());
            ui.painter().hline(sr.left()..=sr.right(), sr.center().y - 2.0, ds);
            ui.painter().hline(sr.left()..=sr.right(), sr.center().y + 2.0, ds);
        };

        // Persistent startup & playback toggles (saved to config).
        double_rule(ui);
        ui.label(RichText::new("Startup & playback").color(theme::SILVER).strong());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Autoplay: lit when ON (the default); click to make clips start paused.
            if widgets::icon_button(
                ui,
                Icon::Play,
                self.autoplay,
                "Autoplay new videos & audio (on by default) — off makes them start paused",
            )
            .clicked()
            {
                self.autoplay = !self.autoplay;
            }
            // Sound on startup: lit when ON; off (default) launches muted.
            if widgets::icon_button(
                ui,
                Icon::Speaker,
                self.sound_on_startup,
                "Sound on startup — launch with panels unmuted (off by default)",
            )
            .clicked()
            {
                self.sound_on_startup = !self.sound_on_startup;
            }
            // Four-panel default: lit when ON; off (default) launches single.
            if widgets::icon_button(
                ui,
                Icon::FourPanels,
                self.four_panel_default,
                "Open with four panels when launched empty (off = single panel)",
            )
            .clicked()
            {
                self.four_panel_default = !self.four_panel_default;
            }
            // Auto-hide cursor: lit when ON (on by default); only hides in
            // fullscreen / maximized, over content, after a still pause.
            if widgets::icon_button(
                ui,
                Icon::MouseHide,
                self.mouse_hide,
                "Auto-hide the cursor (fullscreen / maximized only, over content)",
            )
            .clicked()
            {
                self.mouse_hide = !self.mouse_hide;
            }
            // Presentation cover freezes content: lit when ON.
            if widgets::icon_button(
                ui,
                Icon::Pause,
                self.cover_freezes,
                "Presentation cover freezes content — pauses every panel while up",
            )
            .clicked()
            {
                self.cover_freezes = !self.cover_freezes;
            }
            // Loop folder: lit when ON (moved here from the menu's top row —
            // it's a playback setting, not a panel action).
            if widgets::icon_button(
                ui,
                Icon::Loop,
                self.loop_enabled,
                "Loop folder — wrap the last item back to the first",
            )
            .clicked()
            {
                self.loop_enabled = !self.loop_enabled;
            }
        });

        // A handful of tasteful starting points (colour + opacity).
        const PRESETS: &[(u8, u8, u8, u8)] = &[
            (0x0D, 0x14, 0x20, 0xA6), // default navy
            (0x00, 0x00, 0x00, 0xD2), // near-black
            (0x18, 0x1B, 0x20, 0xBE), // charcoal
            (0x06, 0x22, 0x28, 0xA6), // deep teal
            (0x1A, 0x24, 0x3A, 0xAF), // slate blue
            (0x24, 0x16, 0x12, 0xB0), // warm dark
        ];

        // Divider before the tint controls.
        double_rule(ui);
        ui.label(RichText::new("Background tint").color(theme::SILVER).strong());

        // Preset swatches + reset.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (r, g, b, a) in PRESETS {
                let sw = ui.add(
                    egui::Button::new("")
                        .fill(Color32::from_rgb(*r, *g, *b))
                        .min_size(egui::vec2(24.0, 18.0))
                        .rounding(egui::Rounding::same(3.0)),
                );
                if sw.clicked() {
                    self.bg_hsva = egui::ecolor::Hsva::from(Color32::from_rgb(*r, *g, *b));
                    self.bg_alpha = *a;
                }
            }
            if ui.button("Reset").clicked() {
                let c = crate::config::default_bg_color();
                self.bg_hsva = egui::ecolor::Hsva::from(Color32::from_rgb(c[0], c[1], c[2]));
                self.bg_alpha = crate::config::default_bg_alpha();
            }
        });

        // Colour square + hue (edits HSVA directly → no drift) …
        egui::color_picker::color_picker_hsva_2d(
            ui,
            &mut self.bg_hsva,
            egui::color_picker::Alpha::Opaque,
        );
        // … and a separate opacity/transparency slider. Opacity only does
        // anything through the acrylic layer, so gray it out (with a hint)
        // when this machine has no glass.
        ui.add_enabled(
            self.glass,
            egui::Slider::new(&mut self.bg_alpha, 0..=255)
                // Colour the label so it reads on the dark menu (the default
                // slider text is the near-black ink used by the white menus).
                .text(RichText::new("Opacity").color(theme::SILVER))
                .trailing_fill(true),
        );
        if !self.glass {
            ui.label(
                RichText::new("Opacity needs Windows transparency")
                    .color(theme::SILVER)
                    .size(10.0),
            );
        }

        // Divider before the text-colour controls.
        double_rule(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new("File name text").color(theme::SILVER).strong());
            // A live swatch of the current text colour.
            let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 16.0), Sense::hover());
            ui.painter()
                .rect_filled(r, egui::Rounding::same(3.0), Color32::from(self.text_hsva));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Reset").clicked() {
                    let c = crate::config::default_text_color();
                    self.text_hsva = egui::ecolor::Hsva::from(Color32::from_rgb(c[0], c[1], c[2]));
                }
            });
        });
        // Same square/hue picker as the background (drift-free).
        egui::color_picker::color_picker_hsva_2d(
            ui,
            &mut self.text_hsva,
            egui::color_picker::Alpha::Opaque,
        );
        ui.label(
            RichText::new("Colours the item names in Gallery management.")
                .color(theme::SILVER)
                .size(10.0),
        );

        // About — a plain clickable line at the very bottom of Settings.
        double_rule(ui);
        let about = ui.add(
            egui::Label::new(RichText::new("About MulVie").color(theme::SILVER).size(12.0))
                .sense(Sense::click()),
        );
        if about.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            let r = about.rect;
            ui.painter().hline(
                r.left()..=r.right(),
                r.bottom() - 1.0,
                Stroke::new(1.0_f32, theme::SILVER),
            );
        }
        if about.clicked() {
            if self.about_open {
                // Already open, possibly buried behind other windows — a
                // second click must RAISE it, not silently do nothing.
                ui.ctx().send_viewport_cmd_to(
                    egui::ViewportId::from_hash_of("mulvie_about"),
                    egui::ViewportCommand::Focus,
                );
            }
            self.about_open = true;
            self.bg_menu_open = false; // the menu's job is done
        }
    }

    /// The MulVie menu's expandable Library section: a name/search field with an
    /// inline save (+) button and a scrolling list of saved libraries. Load a
    /// library by double-clicking a row or via its right-click menu (which also
    /// offers Rename / Re-write / Delete). `target_h` is the Settings section's
    /// height, which the list pads to so the two menus are the same size.
    fn bg_menu_library_section(&mut self, ui: &mut egui::Ui, target_h: f32) {
        const NAME_MAX: usize = 14;
        // Width of the widest possible 14-char name in the row font — used to
        // size the name field AND the selection frame, so the frame is "just big
        // enough" for any 14 chars rather than spanning the whole row.
        let name_font = egui::FontId::proportional(14.0);
        let w14 = ui
            .painter()
            .layout_no_wrap("W".repeat(NAME_MAX), name_font.clone(), theme::SILVER)
            .size()
            .x;
        let chip_w = w14 + 16.0;

        let y0 = ui.min_rect().bottom(); // section top — the list pads to target_h

        let ds = Stroke::new(1.0_f32, theme::ACCENT_DIM);
        let w = ui.available_width();
        let (sr, _) = ui.allocate_exact_size(egui::vec2(w, 9.0), Sense::hover());
        ui.painter().hline(sr.left()..=sr.right(), sr.center().y - 2.0, ds);
        ui.painter().hline(sr.left()..=sr.right(), sr.center().y + 2.0, ds);
        ui.label(RichText::new("Library").color(theme::SILVER).strong());

        // Name / search field (only as wide as a 14-char name needs — names are
        // capped at 14 — so it doesn't widen the menu) + a compact inline save
        // (+) button with a large "+" glyph.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let te = ui.add(
                egui::TextEdit::singleline(&mut self.lib_search)
                    .hint_text("name / search")
                    .desired_width(w14 + 16.0),
            );
            if self.lib_search.chars().count() > NAME_MAX {
                self.lib_search = self.lib_search.chars().take(NAME_MAX).collect();
            }
            let name = self.lib_search.trim().to_string();
            let can_save = !name.is_empty() && !self.library_exists(&name);
            let h = te.rect.height();
            if widgets::text_button_sized(ui, "+", true, can_save, egui::vec2(26.0, h), 20.0)
                .on_hover_text("Save the current panels as a new library")
                .clicked()
                && can_save
            {
                self.save_library(&name);
            }
        });
        if self.lib_save_error {
            // The last save/rename/delete never reached the disk (read-only or
            // yanked stick) — without this the change would silently vanish on
            // the next launch while looking saved in the list.
            ui.label(
                RichText::new("⚠ Couldn't write to the stick — changes won't survive a restart.")
                    .color(Color32::from_rgb(0xE0, 0x9A, 0x4A))
                    .size(10.0),
            );
        }

        // The filtered list of saved libraries.
        let filter = self.lib_search.trim().to_lowercase();
        let names: Vec<String> = self
            .libraries
            .iter()
            .filter(|l| filter.is_empty() || l.name.to_lowercase().contains(&filter))
            .map(|l| l.name.clone())
            .collect();

        let mut load: Option<String> = None;
        let mut rename: Option<String> = None;
        let mut rewrite: Option<String> = None;
        let mut delete: Option<String> = None;

        // The list is capped at `list_h` so it does NOT grow the menu as more
        // libraries are added — it scrolls instead. The scrollbar is non-floating
        // and shown whenever the list overflows (not only while hovering). Any
        // slack below the list is padded (further down) so the menu is as tall as
        // Settings even with few / no libraries.
        let used = ui.min_rect().bottom() - y0 + ui.spacing().item_spacing.y;
        let list_h = (target_h - used).max(60.0);
        ui.style_mut().spacing.scroll.floating = false;
        egui::ScrollArea::vertical()
            .id_salt("mulvie_lib_list")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            .max_height(list_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if names.is_empty() {
                    ui.label(
                        RichText::new(if self.libraries.is_empty() {
                            "No libraries yet — type a name and press +."
                        } else {
                            "No match."
                        })
                        .color(theme::SILVER)
                        .size(11.0),
                    );
                }
                for name in &names {
                    let selected = self.lib_selected.as_deref() == Some(name.as_str());
                    // The whole row is clickable, but the highlight (hover fill /
                    // selected FRAME) is only as wide as a 14-char name — a chip,
                    // left-aligned, not the whole row. The name stays silver so
                    // it's easy to read either way.
                    let full_w = ui.available_width();
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(full_w, 22.0), Sense::click());
                    let chip = Rect::from_min_size(
                        rect.left_top(),
                        egui::vec2(chip_w.min(full_w), rect.height()),
                    );
                    {
                        let p = ui.painter();
                        if selected {
                            p.rect_stroke(
                                chip.shrink(1.0),
                                egui::Rounding::same(4.0),
                                Stroke::new(1.5_f32, theme::ACCENT),
                            );
                        } else if resp.hovered() {
                            p.rect_filled(chip, egui::Rounding::same(4.0), theme::PANEL_STRONG);
                        }
                        p.text(
                            rect.left_center() + egui::vec2(8.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            name,
                            name_font.clone(),
                            theme::SILVER,
                        );
                    }
                    if resp.clicked() {
                        self.lib_selected = Some(name.clone());
                    }
                    if resp.double_clicked() {
                        load = Some(name.clone());
                    }
                    resp.context_menu(|ui| {
                        ui.set_min_width(150.0);
                        if ui.button("Load library").clicked() {
                            load = Some(name.clone());
                            ui.close_menu();
                        }
                        if ui.button("Rename…").clicked() {
                            rename = Some(name.clone());
                            ui.close_menu();
                        }
                        if ui.button("Re-write library").clicked() {
                            rewrite = Some(name.clone());
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Delete library").clicked() {
                            delete = Some(name.clone());
                            ui.close_menu();
                        }
                    });
                }
            });

        // Pad the section down to the Settings height so the two menus are the
        // same size regardless of how many libraries there are.
        let grown = ui.min_rect().bottom() - y0;
        let pad = target_h - grown;
        if pad > 0.0 {
            ui.allocate_space(egui::vec2(1.0, pad));
        }

        // (No stand-alone "Load library" button — loading is done by
        // double-clicking a row or via its right-click menu.)

        // Apply the collected actions (after the borrow of the list ends).
        if let Some(n) = load {
            if let Some(lib) = self.libraries.iter().find(|l| l.name == n).cloned() {
                self.apply_library(&lib);
                self.lib_selected = Some(n);
            }
        }
        if let Some(n) = rename {
            self.lib_rename = Some((n.clone(), n));
        }
        if let Some(n) = rewrite {
            self.lib_confirm = Some(LibConfirm::Rewrite(n));
        }
        if let Some(n) = delete {
            self.lib_confirm = Some(LibConfirm::Delete(n));
        }
    }

    /// Draw the Library rename window / confirm prompts (app-styled, centred,
    /// on top of the menu). Returns true while one is up so the menu doesn't
    /// close underneath it.
    fn lib_modals(&mut self, ctx: &Context) -> bool {
        const NAME_MAX: usize = 14;
        let screen = ctx.screen_rect();
        // A full-screen catcher (same Tooltip order as the popup, drawn first
        // so the popup sits on top) that dismisses the modal on an outside click.
        let catcher = |ctx: &Context| -> bool {
            egui::Area::new(Id::new("mulvie_lib_modal_catcher"))
                .order(egui::Order::Tooltip)
                .fixed_pos(screen.min)
                .constrain(false)
                .show(ctx, |ui| ui.allocate_rect(screen, Sense::click()))
                .inner
                .clicked()
        };

        // Rename window.
        if let Some((old, cur)) = self.lib_rename.clone() {
            let mut newname = cur;
            let mut accept = false;
            let mut cancel = catcher(ctx);
            egui::Area::new(Id::new("mulvie_lib_rename"))
                .order(egui::Order::Tooltip)
                .fixed_pos(screen.center() - egui::vec2(150.0, 55.0))
                .show(ctx, |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_max_width(300.0);
                        ui.label(RichText::new("Rename library").color(theme::INK_BLUE).strong());
                        ui.add(egui::TextEdit::singleline(&mut newname).desired_width(220.0));
                        if newname.chars().count() > NAME_MAX {
                            newname = newname.chars().take(NAME_MAX).collect();
                        }
                        let trimmed = newname.trim();
                        let taken = trimmed != old && self.library_exists(trimmed);
                        let ok = !trimmed.is_empty() && !taken;
                        ui.horizontal(|ui| {
                            if widgets::text_button(ui, "Rename", true, ok).clicked() && ok {
                                accept = true;
                            }
                            if widgets::text_button(ui, "Cancel", false, true).clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            if accept {
                self.rename_library(&old, newname.trim());
                self.lib_rename = None;
            } else if cancel {
                self.lib_rename = None;
            } else {
                self.lib_rename = Some((old, newname)); // keep edits across frames
            }
            return true;
        }

        // Confirm prompts (re-write / delete).
        let conf = self.lib_confirm.as_ref().map(|c| match c {
            LibConfirm::Rewrite(n) => (true, n.clone()),
            LibConfirm::Delete(n) => (false, n.clone()),
        });
        if let Some((is_rewrite, name)) = conf {
            let msg = if is_rewrite {
                format!("Re-write \"{name}\" with the current panels?")
            } else {
                format!("Delete library \"{name}\"?  (The content itself is not deleted.)")
            };
            let mut yes = false;
            let mut cancel = catcher(ctx);
            egui::Area::new(Id::new("mulvie_lib_confirm"))
                .order(egui::Order::Tooltip)
                .fixed_pos(screen.center() - egui::vec2(160.0, 45.0))
                .show(ctx, |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_max_width(320.0);
                        ui.label(RichText::new(msg).color(theme::INK_BLUE).strong());
                        ui.separator();
                        ui.horizontal(|ui| {
                            let label = if is_rewrite { "Re-write" } else { "Delete" };
                            if widgets::text_button(ui, label, true, true).clicked() {
                                yes = true;
                            }
                            if widgets::text_button(ui, "Cancel", false, true).clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            if yes {
                if is_rewrite {
                    self.rewrite_library(&name);
                } else {
                    self.delete_library(&name);
                }
                self.lib_confirm = None;
            } else if cancel {
                self.lib_confirm = None;
            }
            return true;
        }
        false
    }

    /// The About window: an app-styled frameless child window (same chrome +
    /// acrylic pattern as the Gallery-management windows) with the version,
    /// the GitHub link, and a concise shortcut/feature reference.
    fn show_about(&mut self, ctx: &Context) {
        const TITLE: &str = "MulVie — About"; // unique OS title (acrylic lookup)
        const SIZE: [f32; 2] = [470.0, 660.0];

        // Centre over the main window ON OPEN only — the stored position must
        // stay CONSTANT across frames (see the `about_pos` field docs). The
        // top-left is clamped on-screen: a short main window parked at the top
        // could otherwise centre the 660-tall About with its titlebar (the
        // only drag handle + Close button) above the visible screen.
        if self.about_pos.is_none() {
            let monitor = ctx.input(|i| i.viewport().monitor_size);
            self.about_pos = ctx.input(|i| i.viewport().outer_rect).map(|outer| {
                let mut p = outer.center() - egui::vec2(SIZE[0] * 0.5, SIZE[1] * 0.5);
                if let Some(m) = monitor {
                    p.x = p.x.min(m.x - SIZE[0]);
                    p.y = p.y.min(m.y - SIZE[1]);
                }
                p.x = p.x.max(0.0);
                p.y = p.y.max(0.0);
                p
            });
        }
        let mut builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(SIZE)
            .with_min_inner_size(SIZE)
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(cfg!(windows)); // acrylic is Windows-only
        if let Some(pos) = self.about_pos {
            builder = builder.with_position(pos);
        }

        let vp_id = egui::ViewportId::from_hash_of("mulvie_about");
        let mut close = false;
        ctx.show_viewport_immediate(vp_id, builder, |vctx, _class| {
            // Fresh native window → (re)apply acrylic, LM-window style; also
            // RE-apply whenever the user's tint changed, so the About window
            // retints live like every other window.
            let tint = self.bg_tint_abgr();
            if (!self.about_glass || self.about_applied_abgr != Some(tint))
                && self.about_glass_attempts <= 240
            {
                self.about_glass_attempts += 1;
                if let Some(hwnd) = crate::os::find_window_by_title(TITLE) {
                    if crate::os::enable_acrylic(hwnd, tint) {
                        self.about_glass = true;
                        self.about_applied_abgr = Some(tint);
                    }
                }
            }
            if vctx.input(|i| i.key_pressed(Key::Escape) || i.viewport().close_requested()) {
                close = true;
            }

            egui::TopBottomPanel::top("about_titlebar")
                .exact_height(34.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme::HEADER_BG)
                        .inner_margin(egui::Margin::symmetric(10.0, 0.0)),
                )
                .show(vctx, |ui| {
                    let bar = ui.interact(
                        ui.max_rect(),
                        Id::new("about_titlebar_bg"),
                        Sense::click_and_drag(),
                    );
                    if bar.drag_started_by(egui::PointerButton::Primary) {
                        vctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        forget_pointer_after_wm_drag(vctx);
                    }
                    ui.horizontal_centered(|ui| {
                        widgets::logo(ui, self.logo_tex.as_ref());
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new("About MulVie")
                                .color(theme::SILVER)
                                .strong()
                                .size(15.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if widgets::window_button(ui, Icon::Close, true, "Close").clicked() {
                                close = true;
                            }
                        });
                    });
                });

            let fill = if self.about_glass {
                Color32::TRANSPARENT
            } else {
                self.bg_rgb()
            };
            // Body text follows the user's text-colour setting live.
            let text = Color32::from(self.text_hsva);
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(fill).inner_margin(egui::Margin::same(16.0)))
                .show(vctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        about_body(ui, text);
                    });
                });
        });

        if close {
            self.about_open = false;
            // A reopened window is a NEW native window: acrylic must reapply,
            // and it re-centres over wherever the main window is THEN.
            self.about_glass = false;
            self.about_glass_attempts = 0;
            self.about_applied_abgr = None;
            self.about_pos = None;
        }
    }

    fn header(&mut self, ctx: &Context) {
        egui::TopBottomPanel::top("mulvie_header")
            .exact_height(HEADER_HEIGHT)
            .frame(
                egui::Frame::none()
                    .fill(theme::HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(10.0, 0.0)),
            )
            .show(ctx, |ui| {
                // The header doubles as the (frameless) window's titlebar:
                // drag empty areas to move the window, double-click toggles
                // fullscreen. Buttons added later sit on top and take their own
                // clicks, so they don't start a window drag.
                let bg = ui.interact(
                    ui.max_rect(),
                    Id::new("mulvie_header_bg"),
                    Sense::click_and_drag(),
                );
                if bg.double_clicked() {
                    self.toggle_fullscreen(ctx);
                } else if bg.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    forget_pointer_after_wm_drag(ctx);
                }

                // Same latch as the panes: a click that dismisses an open
                // context menu must not also fire a header button (incl. Close).
                let suppressed = self.suppress_clicks;

                ui.horizontal_centered(|ui| {
                    // Logo + wordmark, drawn directly in the centred row so they
                    // sit vertically centred; their union rect is the click target
                    // that toggles the MulVie menu. A drag on it moves the window,
                    // like the rest of the header.
                    let logo_r = widgets::logo(ui, self.logo_tex.as_ref());
                    ui.add_space(7.0);
                    let word_r = ui.label(
                        RichText::new("MulVie").color(theme::SILVER).strong().size(16.0),
                    );
                    let brand = ui.interact(
                        logo_r.rect.union(word_r.rect),
                        Id::new("mulvie_brand"),
                        Sense::click_and_drag(),
                    );
                    if brand.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if brand.clicked() && !suppressed {
                        self.bg_menu_open = !self.bg_menu_open;
                        // A (re)opened menu starts its section at the top.
                        self.menu_scroll_reset = self.bg_menu_open;
                    } else if brand.drag_started_by(egui::PointerButton::Primary) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        forget_pointer_after_wm_drag(ctx);
                    }
                    self.menu_anchor = brand.rect.left_bottom() + egui::vec2(0.0, 5.0);
                    self.brand_rect = brand.rect; // so a chrome click elsewhere closes the menu

                    // List Management toggle, just right of the wordmark: with
                    // no browsers open it opens the first (standalone); with
                    // browsers open it HIDES/SHOWS them all — state kept.
                    // Closing an instance for real is its own X button.
                    ui.add_space(10.0);
                    let lm_active = !self.list_mgrs.is_empty() && !self.lm_hidden;
                    let lm_tip = if self.list_mgrs.is_empty() {
                        "Gallery management — browse a panel's folder and manage its files"
                    } else if self.lm_hidden {
                        "Show the Gallery management browsers"
                    } else {
                        "Hide the Gallery management browsers (they keep their state)"
                    };
                    let lm_resp = widgets::icon_button(ui, Icon::ListManage, lm_active, lm_tip);
                    self.lm_icon_rect = lm_resp.rect;
                    if lm_resp.clicked() && !suppressed {
                        if self.list_mgrs.is_empty() {
                            // Preselect the first visible panel with content.
                            let visible: Vec<usize> = self
                                .layout
                                .pane_rects(self.content_area(ctx))
                                .iter()
                                .map(|(q, _)| *q as usize)
                                .collect();
                            let target = visible
                                .iter()
                                .copied()
                                .find(|&i| self.panes[i].current_path().is_some())
                                .or_else(|| visible.first().copied())
                                .unwrap_or(0);
                            self.lm_hidden = false;
                            self.spawn_lm(None, target);
                        } else {
                            let hidden = self.lm_hidden;
                            self.set_lm_hidden(!hidden);
                        }
                    }
                    // The "no fifth browser" hint: a gentle diffused red glow
                    // over the icon, fading out over ~0.7s.
                    if let Some(t0) = self.lm_flash {
                        let t = ((ctx.input(|i| i.time) - t0) / 0.7) as f32;
                        if t >= 1.0 {
                            self.lm_flash = None;
                        } else {
                            let painter = ctx.layer_painter(egui::LayerId::new(
                                egui::Order::Foreground,
                                Id::new("lm_flash_glow"),
                            ));
                            radial_glow(
                                &painter,
                                self.lm_icon_rect.expand(26.0),
                                t,
                                Color32::from_rgb(0xD0, 0x40, 0x38),
                                0.55,
                                0.5,
                            );
                            ctx.request_repaint();
                        }
                    }

                    // (Rename now lives in the MulVie menu, alongside the toggles.)
                    ui.add_space(12.0);
                    ui.spacing_mut().item_spacing.x = 3.0;
                    // Play-all / Pause-all.
                    // Play/pause/stop ALL — locked panes are skipped.
                    if widgets::icon_button(ui, Icon::Play, false, "Resume all videos").clicked()
                        && !suppressed
                    {
                        for i in 0..4 {
                            if !self.panes[i].locked {
                                if let Some(v) = &mut self.videos[i] {
                                    v.user_paused = false;
                                }
                            }
                        }
                    }
                    if widgets::icon_button(ui, Icon::Pause, false, "Pause all videos").clicked()
                        && !suppressed
                    {
                        for i in 0..4 {
                            if !self.panes[i].locked {
                                if let Some(v) = &mut self.videos[i] {
                                    v.user_paused = true;
                                }
                            }
                        }
                    }
                    if widgets::icon_button(ui, Icon::Stop, false, "Stop all videos (rewind + pause)")
                        .clicked()
                        && !suppressed
                    {
                        for i in 0..4 {
                            if !self.panes[i].locked {
                                if let Some(v) = &mut self.videos[i] {
                                    v.stop();
                                }
                            }
                        }
                    }
                    ui.add_space(8.0);
                    // Per-pane audio, numbered in reading order: TL, TR, BL, BR.
                    const MAP: [usize; 4] = [0, 2, 1, 3];
                    const WHERE: [&str; 4] = ["top-left", "top-right", "bottom-left", "bottom-right"];
                    for b in 0..4 {
                        let pane = MAP[b];
                        let tip = format!(
                            "Audio {} ({}) — click to {}",
                            b + 1,
                            WHERE[b],
                            if self.muted[pane] { "unmute" } else { "mute" }
                        );
                        if widgets::mute_button(ui, (b as u8) + 1, self.muted[pane], &tip).clicked()
                            && !suppressed
                        {
                            self.muted[pane] = !self.muted[pane];
                            if let Some(v) = &mut self.videos[pane] {
                                v.set_muted(self.muted[pane]);
                            }
                        }
                    }
                    // Loop / mouse-hide / frost / clear now live in the MulVie
                    // menu (click the wordmark) to keep the chrome uncluttered.

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        // Window controls (rightmost): close, maximize/restore,
                        // minimize. right_to_left => first added sits furthest right.
                        if widgets::window_button(ui, Icon::Close, true, "Close").clicked() && !suppressed {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                        let (mi, mt) = if maximized {
                            (Icon::Restore, "Restore")
                        } else {
                            (Icon::Maximize, "Maximize")
                        };
                        if widgets::window_button(ui, mi, false, mt).clicked() && !suppressed {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                        }
                        if widgets::window_button(ui, Icon::Minimize, false, "Minimize").clicked() && !suppressed {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        ui.add_space(8.0);
                        if widgets::icon_button(ui, Icon::Fullscreen, false, "Fullscreen  (F11)")
                            .clicked()
                            && !suppressed
                        {
                            self.toggle_fullscreen(ctx);
                        }
                        let hidden = !self.show_dividers;
                        let (icon, tip) = if self.show_dividers {
                            (Icon::DividersShown, "Hide divider lines  (H)")
                        } else {
                            (Icon::DividersHidden, "Show divider lines  (H)")
                        };
                        if widgets::icon_button(ui, icon, hidden, tip).clicked() && !suppressed {
                            self.toggle_dividers();
                        }
                        // The 1 ↔ 4 layout slide switch; pane contents are
                        // untouched either way.
                        let multi = self
                            .layout
                            .pane_rects(self.content_area(ctx))
                            .len()
                            > 1;
                        let tip = if multi {
                            "Single view — top-left panel fills the window  (G)"
                        } else {
                            "MultiView — split into four equal panels  (G)"
                        };
                        if widgets::view_toggle(ui, multi, tip).clicked() && !suppressed {
                            self.toggle_view_mode(ctx);
                        }
                        ui.add_space(8.0);
                        // Presentation cover (frost): hide every panel behind the
                        // app background. Optionally freezes content (Settings).
                        if widgets::icon_button(ui, Icon::Frost, self.frost_all, "Presentation cover  (Shift+H)")
                            .clicked()
                            && !suppressed
                        {
                            self.toggle_cover();
                        }
                    });
                });

                // Steel-blue accent underline.
                let r = ui.max_rect();
                ui.painter().hline(
                    r.left()..=r.right(),
                    r.bottom() - 0.5,
                    Stroke::new(1.0_f32,theme::ACCENT),
                );
            });
    }

    // --- Persistence -----------------------------------------------------

    fn to_config(&self, ctx: &Context) -> Config {
        // Deliberately excludes pane folders/files and the layout — nothing
        // about what was being viewed survives a restart.
        let size = ctx.screen_rect().size();
        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        Config {
            window_size: Some([size.x, size.y]),
            maximized,
            loop_enabled: self.loop_enabled,
            mouse_hide: self.mouse_hide,
            bg_color: {
                let c = self.bg_rgb();
                [c.r(), c.g(), c.b()]
            },
            bg_alpha: self.bg_alpha,
            text_color: {
                let c = Color32::from(self.text_hsva);
                [c.r(), c.g(), c.b()]
            },
            autoplay: self.autoplay,
            sound_on_startup: self.sound_on_startup,
            four_panel_default: self.four_panel_default,
            cover_freezes: self.cover_freezes,
        }
    }

    fn maybe_save(&mut self, ctx: &Context) {
        let cfg = self.to_config(ctx);
        let Ok(json) = serde_json::to_string_pretty(&cfg) else {
            return;
        };
        if json == self.last_saved_json {
            return;
        }
        let now = ctx.input(|i| i.time);
        if now - self.last_save_time < 0.8 {
            // Debounce; ensure we get repainted to flush later.
            ctx.request_repaint_after(std::time::Duration::from_millis(900));
            return;
        }
        crate::config::save(&cfg);
        self.last_saved_json = json;
        self.last_save_time = now;
    }
}

impl eframe::App for MulVieApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Release any keep-awake claim. On Linux this kills the
        // systemd-inhibit child — without it, quitting mid-playback would
        // leave the machine's sleep blocked forever.
        crate::os::set_keep_awake(false, false, self.hwnd);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.glass {
            [0.0, 0.0, 0.0, 0.0] // transparent -> acrylic blur shows through
        } else {
            // Match the CentralPanel fill (canvas_color) so a resize-exposed
            // edge doesn't flash the old fixed navy before egui repaints.
            let c = self.bg_rgb();
            [
                c.r() as f32 / 255.0,
                c.g() as f32 / 255.0,
                c.b() as f32 / 255.0,
                1.0,
            ]
        }
    }

    fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        // Neutralise egui's built-in ctrl/keyboard UI zoom; we do our own.
        ctx.set_zoom_factor(1.0);
        // Linux: a finished WM move/resize can leave the resize arrows stuck
        // on the window (cursor-cache desync, see forget_pointer_after_wm_drag
        // and os::define_cursor_default). When the flag is up and pointer
        // events flow again (the WM grab is over), clear the cursor directly.
        if !cfg!(windows) {
            let fix = Id::new("mulvie_wm_cursor_fix");
            if ctx.data(|d| d.get_temp::<bool>(fix).unwrap_or(false))
                && ctx.input(|i| i.pointer.latest_pos().is_some())
            {
                ctx.data_mut(|d| d.remove::<bool>(fix));
                if let Some(w) = self.hwnd {
                    crate::os::define_cursor_default(w);
                }
            }
        }
        // Context-menu dismissal: egui closes a popup on the pointer *press*,
        // but a pane's click only resolves on the later *release*. So latch the
        // suppression on the press that dismisses a menu and hold it until the
        // next ordinary press — that way the dismissing click does nothing else.
        // NOTE: egui context menus (`Response::context_menu`) are NOT tracked by
        // `Memory::any_popup_open()` — that flag only covers combo boxes, color
        // pickers and `popup_below_widget`. A context menu lives in a separate
        // `BarState` persisted in `ctx.data` under this id, so query THAT; using
        // `any_popup_open()` here always returned false and never suppressed.
        // The menu-open state is shared by ALL viewports. A menu living in a
        // standalone List-Management window must not latch the MAIN window's
        // suppression: main-window clicks can never close that menu, so the
        // latch would swallow every click until the user returned to the
        // browser window. Each browser tracks ownership (`menu_owns`).
        let lm_menu_elsewhere = self
            .list_mgrs
            .iter()
            .any(|m| m.pinned.is_none() && m.menu_owns);
        let popup_open = egui::menu::BarState::load(ctx, Id::new("__egui::context_menu")).is_some()
            && !lm_menu_elsewhere;
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.suppress_clicks = popup_open || self.popup_open_prev;
        }
        self.popup_open_prev = popup_open;

        // Cross-instance List-Management rules, synced every frame: which
        // panels host a pin (they take no content), and per instance which
        // panels are already managed or pinned by ANOTHER instance.
        {
            let pins: Vec<Option<usize>> = self.list_mgrs.iter().map(|m| m.pinned).collect();
            let targets: Vec<usize> = self.list_mgrs.iter().map(|m| m.target).collect();
            let mut pinned_mask = [false; 4];
            for p in pins.iter().flatten() {
                pinned_mask[*p] = true;
            }
            for (k, lm) in self.list_mgrs.iter_mut().enumerate() {
                for p in 0..4 {
                    lm.taken_targets[p] =
                        targets.iter().enumerate().any(|(j, &t)| j != k && t == p);
                    lm.taken_pins[p] = pins
                        .iter()
                        .enumerate()
                        .any(|(j, &pin)| j != k && pin == Some(p));
                }
            }
            for p in &mut self.panes {
                p.lm_occupied = pinned_mask;
            }
        }

        // Mirror the global loop toggle into every pane; a closed menu also
        // disarms any pending two-step delete (click-elsewhere/Esc = cancel).
        for p in &mut self.panes {
            p.loop_folder = self.loop_enabled;
            if !popup_open {
                p.delete_armed = false;
                p.delete_target = None;
            }
        }
        // Heartbeat so the open-with inbox is noticed even while idle
        // (eframe otherwise only repaints on input).
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
        self.poll_inbox(ctx);
        self.poll_dialog(ctx);
        self.try_enable_glass(ctx, frame);
        self.apply_bg_tint(); // live-apply any change from the tint menu
        self.sync_videos(ctx, frame);
        self.store.poll(ctx);
        self.thumbs.poll(ctx);
        self.handle_keys(ctx);

        if !self.fullscreen {
            self.header(ctx);
            self.show_bg_menu(ctx);
        }

        let bg = self.canvas_color();
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let rects = self.layout.pane_rects(area);

                // Route scroll + drops + shortcuts before drawing so they show
                // this frame.
                self.handle_scroll(ctx, &rects);
                self.handle_file_drop(ctx, &rects);
                self.handle_shortcuts(ctx, &rects);
                self.handle_fullscreen_dblclick(ctx, &rects);
                self.update_cursor_hide(ctx, &rects);

                // Frost is a presentation cover: it HIDES every pane and the
                // dividers, leaving only the app background (this CentralPanel's
                // fill — the acrylic tint). Content keeps playing behind it; the
                // header/menu stay so the cover can be lifted (Shift+H or the
                // menu). Standalone Gallery windows are their own windows and
                // are untouched.
                let frosted = self.frost_all;
                if !frosted {
                for (quad, rect) in &rects {
                    let idx = *quad as usize;
                    let pinned_here = if self.lm_hidden {
                        None
                    } else {
                        self.lm_pinned_at(idx)
                    };
                    if let Some(k) = pinned_here {
                        // Pinned List Management replaces this panel's view;
                        // the panel's own (frozen) content sits underneath and
                        // returns on unpin.
                        let suppress = self.suppress_clicks;
                        let style = self.lm_style();
                        let MulVieApp {
                            list_mgrs,
                            panes,
                            thumbs,
                            pdfium,
                            ..
                        } = &mut *self;
                        list_manager::pinned_ui(
                            &mut list_mgrs[k],
                            panes,
                            thumbs,
                            pdfium.as_ref(),
                            ui,
                            *rect,
                            suppress,
                            style,
                        );
                    } else if self.pane_is_pdf(idx) {
                        self.show_pdf_pane(ui, ctx, idx, *rect, bg);
                    } else if self.pane_plays(idx) {
                        self.show_video_pane(ui, idx, *rect, bg);
                    } else {
                        self.panes[idx].show(ui, ctx, &mut self.store, *rect, bg, self.suppress_clicks);
                    }
                }

                // Folder-boundary overlays: a blue glass pulse when blocked at an
                // end (loop off), or a fade-in of the new item when it wrapped.
                let now = ctx.input(|i| i.time);
                for i in 0..4 {
                    if let Some(flash) = self.panes[i].last_nav.take() {
                        self.nav_anim[i] = Some((flash, now));
                    }
                }
                for (quad, rect) in &rects {
                    let idx = *quad as usize;
                    if let Some((flash, start)) = self.nav_anim[idx] {
                        let dur = flash.duration();
                        if now - start >= dur {
                            self.nav_anim[idx] = None;
                        } else {
                            ctx.request_repaint();
                            let t = ((now - start) / dur) as f32;
                            let painter = ui.painter_at(*rect);
                            match flash {
                                NavFlash::Blocked => draw_block_glow(&painter, *rect, t),
                                NavFlash::Wrapped => draw_wrap_glow(&painter, *rect, t),
                            }
                        }
                    }
                }

                // "Library content missing" notices: a double red pulse (the
                // Blocked pulse's shape, in red) plus the missing path printed
                // mid-panel. Left click dismisses; right-click offers copying
                // the path. Drawn after the pane content so the interact wins
                // the hit-test over the pane's own widgets.
                for (quad, rect) in &rects {
                    let idx = *quad as usize;
                    let Some(m) = &mut self.lib_missing[idx] else {
                        continue;
                    };
                    // The pulse clock starts on the first VISIBLE frame.
                    let start = *m.pulse_start.get_or_insert(now);
                    let t = now - start;
                    const PULSE: f64 = 0.55; // one hump; two run back-to-back
                    if t < 2.0 * PULSE {
                        ctx.request_repaint();
                        let tt = ((t / PULSE).fract()) as f32;
                        let painter = ui.painter_at(*rect);
                        draw_missing_glow(&painter, *rect, tt);
                    }
                    let (title, hint) = if m.folder_gone {
                        ("Missing folder:", "The panel stays empty.")
                    } else {
                        ("Missing file:", "Showing the folder's first file instead.")
                    };
                    let path_str = m.path.display().to_string();
                    draw_missing_notice(ui, *rect, title, &path_str, hint);

                    // Whole-panel interaction: left click dismisses (gated by
                    // the same latch as pane clicks, so a menu-dismiss click
                    // can't also swallow the notice); right-click = copy menu.
                    let resp = ui.interact(
                        *rect,
                        Id::new(("mulvie_lib_missing", idx)),
                        Sense::click(),
                    );
                    let mut dismiss = resp.clicked() && !self.suppress_clicks;
                    resp.context_menu(|ui| {
                        ui.set_min_width(widgets::MENU_WIDTH);
                        if ui.button("Copy missing path").clicked() {
                            ui.ctx().output_mut(|o| o.copied_text = path_str.clone());
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Dismiss").clicked() {
                            dismiss = true;
                            ui.close_menu();
                        }
                    });
                    if dismiss {
                        self.lib_missing[idx] = None;
                    }
                }

                self.dividers(ui, area);
                } // end: content hidden while frosted
            });

        // The List-Management browsers in their standalone windows (instances
        // not pinned into a panel), unless globally hidden. Immediate
        // viewports: the main window keeps rendering and playing normally.
        if !self.lm_hidden {
            for k in 0..self.list_mgrs.len() {
                if self.list_mgrs[k].pinned.is_some() {
                    continue;
                }
                let slot = self.list_mgrs[k].slot;
                let title = self.list_mgrs[k].title.clone();
                let style = self.lm_style();
                let vp_id = egui::ViewportId::from_hash_of(("mulvie_list_mgr", slot));
                let builder = egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size([660.0, 640.0])
                    .with_min_inner_size([500.0, 420.0])
                    .with_decorations(false)
                    .with_transparent(cfg!(windows)); // acrylic glass (Windows only)
                ctx.show_viewport_immediate(vp_id, builder, |vctx, _class| {
                    {
                        let MulVieApp {
                            list_mgrs,
                            panes,
                            thumbs,
                            pdfium,
                            ..
                        } = &mut *self;
                        list_manager::window_ui(
                            &mut list_mgrs[k],
                            panes,
                            thumbs,
                            pdfium.as_ref(),
                            vctx,
                            style,
                        );
                    }
                    if vctx.input(|i| i.viewport().close_requested()) {
                        self.list_mgrs[k].close_request = true;
                    }
                });
            }
        }
        if self.about_open {
            self.show_about(ctx);
        }
        self.process_lm_drag(ctx);
        self.process_move_batch(ctx);
        self.process_lm_delete(ctx);
        self.show_move_conflict(ctx);
        self.show_lm_delete_confirm(ctx);

        self.show_delete_confirm(ctx);

        if !self.fullscreen {
            self.window_resize(ctx);
        }
        self.process_dialog_requests();
        self.process_delete_requests();
        self.process_switch_requests();
        self.process_lm_requests(ctx);
        self.maybe_save(ctx);
        self.update_keep_awake();

        // Applied last so nothing drawn this frame overrides the hidden cursor.
        if self.cursor_hidden {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        // Called by eframe on shutdown and periodically; flush our own config.
        // (We can't read the live ctx here, so persist what we last knew.)
        if !self.last_saved_json.is_empty() {
            if let Ok(cfg) = serde_json::from_str::<Config>(&self.last_saved_json) {
                crate::config::save(&cfg);
            }
        }
    }
}

fn divider_color(r: &egui::Response) -> Color32 {
    if r.hovered() || r.dragged() {
        theme::DIVIDER_HOVER
    } else {
        theme::DIVIDER
    }
}

/// A radial glow at the pane centre, gradiented from `color` out to
/// transparency, pulsing in then out over the animation (`t` is 0→1 progress).
/// Also reused by Gallery management's refresh pulse and the chrome-icon flash.
pub(crate) fn radial_glow(
    painter: &egui::Painter,
    rect: Rect,
    t: f32,
    color: Color32,
    peak_alpha: f32,
    radius_frac: f32,
) {
    let env = (std::f32::consts::PI * t).sin().clamp(0.0, 1.0); // 0 → 1 → 0
    let alpha = (env * peak_alpha * 255.0) as u8;
    if alpha == 0 {
        return;
    }
    let center = rect.center();
    let radius = radius_frac * rect.width().min(rect.height());
    let core = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
    let edge = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, core);
    let n = 48u32;
    for k in 0..=n {
        let ang = k as f32 / n as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(center + egui::vec2(ang.cos(), ang.sin()) * radius, edge);
    }
    for k in 1..=n {
        mesh.add_triangle(0, k, k + 1);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// "Blocked at a folder boundary" (loop off): a gentle blue glass pulse.
fn draw_block_glow(painter: &egui::Painter, rect: Rect, t: f32) {
    radial_glow(painter, rect, t, theme::ACCENT, 0.45, 0.26);
}

/// "Wrapped to the other end" (loop on): the same pulse, but more prominent
/// and silvery instead of blue.
fn draw_wrap_glow(painter: &egui::Painter, rect: Rect, t: f32) {
    radial_glow(
        painter,
        rect,
        t,
        Color32::from_rgb(0xE6, 0xEC, 0xF6),
        0.62,
        0.34,
    );
}

/// "Library content missing": the Blocked pulse's shape in a warning red
/// (run twice by the caller).
fn draw_missing_glow(painter: &egui::Painter, rect: Rect, t: f32) {
    radial_glow(painter, rect, t, Color32::from_rgb(0xC8, 0x45, 0x38), 0.5, 0.3);
}

/// The About window's content: version + description, the feature and
/// shortcut reference, and the GitHub link at the very bottom. Body text
/// follows the user's text-colour setting (the same one Gallery management
/// item names use); only the hyperlink keeps the accent colour.
fn about_body(ui: &mut egui::Ui, text: Color32) {
    let dim = text.gamma_multiply(0.85); // slightly quieter, for the grids' right column
    let rule = |ui: &mut egui::Ui| {
        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(egui::vec2(w, 9.0), Sense::hover());
        let ds = Stroke::new(1.0_f32, theme::ACCENT_DIM);
        ui.painter().hline(r.left()..=r.right(), r.center().y - 2.0, ds);
        ui.painter().hline(r.left()..=r.right(), r.center().y + 2.0, ds);
    };
    // egui's bundled font has no true bold face (`.strong()` only brightens),
    // so headings are drawn TWICE with a half-pixel offset — real visual bold.
    let bold = |ui: &mut egui::Ui, s: &str, size: f32, col: Color32| {
        let font = egui::FontId::proportional(size);
        let galley = ui
            .painter()
            .layout_no_wrap(s.to_owned(), font, col);
        let (r, _) =
            ui.allocate_exact_size(galley.size() + egui::vec2(1.0, 0.0), Sense::hover());
        ui.painter().galley(r.min, galley.clone(), col);
        ui.painter().galley(r.min + egui::vec2(0.5, 0.0), galley, col);
    };
    let rows = |ui: &mut egui::Ui, id: &str, items: &[(&str, &str)]| {
        egui::Grid::new(id)
            .num_columns(2)
            .spacing(egui::vec2(14.0, 3.0))
            .show(ui, |ui| {
                for (key, what) in items {
                    ui.label(RichText::new(*key).color(text).size(11.5));
                    ui.label(RichText::new(*what).color(dim).size(11.5));
                    ui.end_row();
                }
            });
    };

    // Title + the whole description as ONE paragraph (incl. the privacy and
    // open-source notes), at the very top.
    ui.add_space(2.0);
    bold(ui, &format!("MulVie  v{}", env!("CARGO_PKG_VERSION")), 17.0, text);
    ui.add_space(2.0);
    ui.label(
        RichText::new(
            "Portable multi-viewer: up to four adjustable panels for pictures, \
             video, audio and PDF. No installation needed - works on \
             Linux Mint 22 and Windows 10 & 11. This app protects your \
             privacy - all settings live in the app directory, it can \
             run directly from a USB stick, and it does not write anything to \
             the host PC. MulVie can be set as the \
             default Windows picture or video application, and it is a great \
             substitute for the resource-heavy but feature-poor options that \
             come pre-installed. This app is free & open source. If you like \
             it, please consider sharing it with your friends.",
        )
        .color(text)
        .size(12.0),
    );

    rule(ui);
    bold(ui, "Features", 13.0, text);
    ui.label(
        RichText::new(
            "Gallery management - browse, mark, rename, find duplicates, move or \
             delete files. Library - save and reload whole panel layouts. \
             Presentation cover, per-panel lock and mute, folder loop, playback \
             speed, A-B loop, audio tracks and subtitles.",
        )
        .color(dim)
        .size(11.5),
    );

    rule(ui);
    bold(ui, "Keyboard", 13.0, text);
    rows(
        ui,
        "about_keys",
        &[
            ("F11", "fullscreen (double-click the header works too)"),
            ("Esc", "close / back / leave fullscreen"),
            ("H", "show or hide the divider lines"),
            ("Shift+H", "presentation cover"),
            ("G", "single \u{2194} four panels"),
            ("Shift+C", "clear all panels"),
            ("\u{2190} / \u{2192}", "step every panel (locked panels hold)"),
            ("Space", "pause / resume the hovered panel"),
            ("R / Shift+R", "rotate the hovered panel"),
            ("F / Shift+F", "next / previous file in the hovered panel"),
            ("Delete", "delete the hovered file (with confirmation)"),
            ("Tab (hold)", "reveal dividers, catch an edge-stuck one"),
        ],
    );

    rule(ui);
    bold(ui, "Mouse", 13.0, text);
    rows(
        ui,
        "about_mouse",
        &[
            ("Scroll", "next / previous file"),
            ("Ctrl+Scroll", "zoom (drag to pan while zoomed)"),
            ("Alt+Scroll", "step every panel at once"),
            ("Click edges", "previous / next picture"),
            ("Click video", "pause / resume (hover for seek & volume)"),
            ("Right-click", "the full panel menu"),
            ("Drag & drop", "open files or folders in a panel"),
        ],
    );

    // The link, then the legal disclaimer at the very bottom.
    rule(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Source & releases:").color(text).size(12.0));
        ui.hyperlink_to(
            RichText::new("github.com/Rick-CZE/MulVie").color(theme::ACCENT).size(12.0),
            "https://github.com/Rick-CZE/MulVie",
        );
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "This software is provided \u{201c}as is\u{201d}, without warranty \
             of any kind, express or implied, including but not limited to \
             merchantability or fitness for a particular purpose. In no event \
             shall the author be liable for any claim, damages or other \
             liability arising from the use of this software. The source code \
             is open - users are encouraged to review it before use.",
        )
        .color(dim.gamma_multiply(0.9))
        .size(9.5),
    );
}

/// The mid-panel "library content missing" card: title, the missing path
/// (wrapped), what happened instead, and how to dismiss/copy. Sized to the
/// text, clamped to the panel, red-bordered on a dark glass backdrop.
fn draw_missing_notice(ui: &egui::Ui, rect: Rect, title: &str, path: &str, hint: &str) {
    let painter = ui.painter_at(rect);
    let red = Color32::from_rgb(0xC8, 0x45, 0x38);
    let max_w = (rect.width() - 48.0).clamp(120.0, 460.0);

    let title_job = painter.layout(
        title.to_owned(),
        egui::FontId::proportional(14.0),
        red,
        max_w,
    );
    let path_job = painter.layout(
        path.to_owned(),
        egui::FontId::proportional(13.0),
        theme::BRIGHT,
        max_w,
    );
    let hint_job = painter.layout(
        format!("{hint}\nClick to dismiss · right-click to copy the path"),
        egui::FontId::proportional(10.5),
        theme::SILVER,
        max_w,
    );

    let gap = 6.0;
    let w = title_job
        .size()
        .x
        .max(path_job.size().x)
        .max(hint_job.size().x);
    let h = title_job.size().y + path_job.size().y + hint_job.size().y + 2.0 * gap;
    let card = Rect::from_center_size(rect.center(), egui::vec2(w + 28.0, h + 24.0));

    painter.rect_filled(
        card,
        egui::Rounding::same(7.0),
        Color32::from_rgba_unmultiplied(0x10, 0x16, 0x22, 235),
    );
    painter.rect_stroke(card, egui::Rounding::same(7.0), Stroke::new(1.2_f32, red));

    let mut y = card.top() + 12.0;
    for job in [title_job, path_job, hint_job] {
        let x = card.center().x - job.size().x * 0.5;
        let dy = job.size().y;
        painter.galley(pos2(x, y), job, Color32::WHITE);
        y += dy + gap;
    }
}

/// Move a file to the Recycle Bin; if that's not possible, delete it
/// permanently. Retries briefly because a just-released player (mpv) may
/// still hold the file for a few tens of milliseconds.
fn delete_from_disk(path: &Path) -> bool {
    for _ in 0..10 {
        if !path.exists() {
            return true;
        }
        if trash::delete(path).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    std::fs::remove_file(path).is_ok()
}

/// hh:mm:ss with double digits throughout (e.g. 00:04:07).
fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Silvery-white text with a small, sharp blueish drop shadow for readability
/// over video.
fn shadow_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    font: egui::FontId,
) {
    painter.text(
        pos + egui::vec2(1.2, 1.2),
        anchor,
        text,
        font.clone(),
        Color32::from_rgba_unmultiplied(0x10, 0x28, 0x5A, 230),
    );
    painter.text(pos, anchor, text, font, Color32::from_rgb(0xE9, 0xEE, 0xF7));
}

/// Edge-resize hit band width. 6pt matches the Windows feel; Linux gets a
/// wider band — X11 delivers discrete motion events (and VM pointer
/// integration makes the jumps bigger), so a 6pt band is easy to fly over
/// without a single event ever landing inside it. Costs a few px of the
/// panes' edge-click zones on Linux only.
pub(crate) const RESIZE_BAND: f32 = if cfg!(target_os = "linux") { 8.0 } else { 6.0 };

/// After handing a move/resize to the OS, forget egui's pointer state.
/// On X11/Wayland the window manager takes the pointer grab and the app never
/// receives the button-release (egui #7959) — egui would keep this button
/// "down" forever, leaving a stuck resize cursor / phantom drag until the
/// next click. Real events rebuild the state once the WM lets go. Windows
/// delivers the release itself, so there this must stay hands-off.
pub(crate) fn forget_pointer_after_wm_drag(ctx: &Context) {
    if !cfg!(windows) {
        ctx.input_mut(|i| i.pointer = Default::default());
        // Ask update() to also clear the MAIN window's X11 cursor attribute
        // once pointer events flow again — see os::define_cursor_default.
        ctx.data_mut(|d| d.insert_temp(Id::new("mulvie_wm_cursor_fix"), true));
    }
}

/// The 8 edge/corner resize zones for a window `rect`: edges `bw` thick,
/// corners a LARGER `c`×`c` square — a band-sized corner is a near-unhittable
/// target (especially on X11, where motion arrives in coarse jumps and a
/// diagonal grab needs the pointer inside the outermost band in BOTH axes).
/// Corners are listed AFTER the edges, so their Areas stack on top and win
/// the overlap.
pub(crate) fn resize_zones(rect: Rect, bw: f32) -> [(&'static str, Rect, egui::ResizeDirection, CursorIcon); 8] {
    use egui::ResizeDirection as RD;
    let c = bw * 2.5;
    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    [
        ("n", Rect::from_min_max(pos2(l + c, t), pos2(r - c, t + bw)), RD::North, CursorIcon::ResizeVertical),
        ("s", Rect::from_min_max(pos2(l + c, b - bw), pos2(r - c, b)), RD::South, CursorIcon::ResizeVertical),
        ("w", Rect::from_min_max(pos2(l, t + c), pos2(l + bw, b - c)), RD::West, CursorIcon::ResizeHorizontal),
        ("e", Rect::from_min_max(pos2(r - bw, t + c), pos2(r, b - c)), RD::East, CursorIcon::ResizeHorizontal),
        ("nw", Rect::from_min_max(pos2(l, t), pos2(l + c, t + c)), RD::NorthWest, CursorIcon::ResizeNwSe),
        ("se", Rect::from_min_max(pos2(r - c, b - c), pos2(r, b)), RD::SouthEast, CursorIcon::ResizeNwSe),
        ("ne", Rect::from_min_max(pos2(r - c, t), pos2(r, t + c)), RD::NorthEast, CursorIcon::ResizeNeSw),
        ("sw", Rect::from_min_max(pos2(l, b - c), pos2(l + c, b)), RD::SouthWest, CursorIcon::ResizeNeSw),
    ]
}

/// Place one edge/corner resize Area, pinned to EXACTLY its zone. The three
/// builder calls are load-bearing: without them egui seeds a 600×400 default
/// size and (constrain=true) shoves right/bottom areas inward, ballooning them
/// into huge invisible drag layers that swallow clicks across the window.
pub(crate) fn place_resize_area(
    ctx: &Context,
    prefix: &str,
    name: &str,
    zone: Rect,
    dir: egui::ResizeDirection,
    cursor: CursorIcon,
) {
    egui::Area::new(Id::new(("mulvie_resize", prefix, name)))
        .order(egui::Order::Foreground)
        .fixed_pos(zone.min)
        .default_size(zone.size())
        .constrain(false)
        .movable(false)
        .show(ctx, |ui| {
            let resp = ui.allocate_rect(zone, Sense::drag());
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(cursor);
            }
            if resp.drag_started_by(egui::PointerButton::Primary) {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
                forget_pointer_after_wm_drag(ui.ctx());
            }
        });
}

/// The video hover-control geometry for a pane `rect`: (seek bar, volume bar,
/// seek hit band, volume hit band). One source of truth, shared by the video
/// pane chrome and the fullscreen double-click carve-outs.
fn video_bars(rect: Rect) -> (Rect, Rect, Rect, Rect) {
    let pad = 12.0;
    let bar = Rect::from_min_max(
        pos2(rect.left() + pad, rect.bottom() - pad - 5.0),
        pos2(rect.right() - pad, rect.bottom() - pad),
    );
    let vol_w = (rect.width() * 0.34).clamp(120.0, 280.0);
    // Volume bar sits high in the pane (~top 7%), away from the centre.
    let vol = Rect::from_center_size(
        pos2(rect.center().x, rect.top() + rect.height() * 0.07),
        egui::vec2(vol_w, 5.0),
    );
    (
        bar,
        vol,
        bar.expand2(egui::vec2(0.0, 8.0)),
        vol.expand2(egui::vec2(6.0, 9.0)),
    )
}

/// Whether a double-click at `pos` inside a fullscreen pane may exit
/// fullscreen. Carve-outs (deliberate design): the prev/next side strips of
/// image/PDF panes (rapid click-browsing must not throw the presentation out
/// of fullscreen) and the seek/volume bars of playable panes. A playable
/// pane's CENTRE deliberately allows it (the pause flicks off/on,
/// YouTube-style), and empty panes never exit — their click opens the picker.
fn dblclick_exits_fullscreen(pos: egui::Pos2, rect: Rect, plays: bool, has_content: bool) -> bool {
    if !has_content {
        return false;
    }
    if plays {
        let (_, _, bar_hit, vol_hit) = video_bars(rect);
        !bar_hit.contains(pos) && !vol_hit.contains(pos)
    } else {
        let x = (pos.x - rect.left()) / rect.width().max(1.0);
        (IMG_SIDE..=1.0 - IMG_SIDE).contains(&x)
    }
}

/// Largest rectangle of the given aspect (w/h) centred inside `rect`.
fn contain(rect: Rect, aspect: f32) -> Rect {
    let (pw, ph) = (rect.width(), rect.height());
    let mut w = pw;
    let mut h = pw / aspect;
    if h > ph {
        h = ph;
        w = ph * aspect;
    }
    Rect::from_center_size(rect.center(), egui::vec2(w, h))
}

#[cfg(test)]
mod tests {
    use super::{
        dblclick_exits_fullscreen, fmt_time, place_resize_area, resize_zones, video_bars,
    };
    use eframe::egui::{self, pos2, vec2, Id, LayerId, Order, RawInput, Rect};

    /// The fullscreen double-click exit fires in the safe middle but never on
    /// the nav side strips, the video bars, or an empty pane.
    #[test]
    fn fullscreen_dblclick_carveouts() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 600.0));

        // Image/PDF pane: centre exits, the side strips don't, empty never.
        assert!(dblclick_exits_fullscreen(pos2(500.0, 300.0), rect, false, true));
        assert!(!dblclick_exits_fullscreen(pos2(50.0, 300.0), rect, false, true)); // prev strip
        assert!(!dblclick_exits_fullscreen(pos2(950.0, 300.0), rect, false, true)); // next strip
        assert!(!dblclick_exits_fullscreen(pos2(500.0, 300.0), rect, false, false)); // empty

        // Playable pane: centre exits; the seek and volume bars (and their
        // padded hit bands) don't; there are no side strips, so edges DO exit.
        assert!(dblclick_exits_fullscreen(pos2(500.0, 300.0), rect, true, true));
        let (bar, vol, bar_hit, vol_hit) = video_bars(rect);
        assert!(!dblclick_exits_fullscreen(bar.center(), rect, true, true));
        assert!(!dblclick_exits_fullscreen(vol.center(), rect, true, true));
        assert!(!dblclick_exits_fullscreen(bar_hit.center(), rect, true, true));
        assert!(!dblclick_exits_fullscreen(vol_hit.left_center(), rect, true, true));
        assert!(dblclick_exits_fullscreen(pos2(50.0, 300.0), rect, true, true));
    }

    /// Video time readout: hh:mm:ss, double digits throughout.
    #[test]
    fn time_formats_double_digit() {
        assert_eq!(fmt_time(0.0), "00:00:00");
        assert_eq!(fmt_time(59.9), "00:00:59");
        assert_eq!(fmt_time(3661.2), "01:01:01");
        assert_eq!(fmt_time(7325.0), "02:02:05");
        assert_eq!(fmt_time(-3.0), "00:00:00");
    }

    /// Regression guard for the click-eating resize bands (the frameless-window
    /// interaction bug): after building the edge/corner resize Areas, no point in
    /// the window interior may be captured by a resize layer, and every resize
    /// Area must stay thin. Without the `constrain(false)/default_size/movable`
    /// pinning in `place_resize_area`, the right/bottom Areas balloon to ~600px
    /// and this test fails.
    #[test]
    fn resize_areas_do_not_cover_the_interior() {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0));
        let input = RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        // egui's first pass is a sizing pass; run twice so state stabilises.
        for _ in 0..2 {
            let _ = ctx.run(input.clone(), |ctx| {
                for (name, zone, dir, cursor) in resize_zones(screen, 6.0) {
                    place_resize_area(ctx, "main", name, zone, dir, cursor);
                }
            });
        }

        let names = ["n", "s", "w", "e", "nw", "se", "ne", "sw"];
        let resize_layers: Vec<LayerId> = names
            .iter()
            .map(|n| LayerId::new(Order::Foreground, Id::new(("mulvie_resize", "main", n))))
            .collect();

        // Points spread across the interior must reach through to the content.
        for p in [
            pos2(640.0, 400.0),
            pos2(1000.0, 400.0),
            pos2(640.0, 700.0),
            pos2(1000.0, 700.0),
            pos2(300.0, 600.0),
        ] {
            let layer = ctx.layer_id_at(p);
            assert!(
                layer.map_or(true, |l| !resize_layers.contains(&l)),
                "interior point {p:?} captured by a resize layer: {layer:?}"
            );
        }

        // Each resize Area's stored rect must be thin on at least one side.
        // The id MUST match place_resize_area's ("mulvie_resize", prefix, name)
        // 3-tuple — a stale id makes area_rect return None and silently skips
        // every assertion (which is exactly what happened after the prefix
        // refactor), so a missing rect is now a hard failure.
        for n in names {
            let rect = ctx
                .memory(|m| m.area_rect(Id::new(("mulvie_resize", "main", n))))
                .unwrap_or_else(|| panic!("resize area {n} not found — id drifted?"));
            assert!(
                rect.width() <= 20.0 || rect.height() <= 20.0,
                "resize area {n} ballooned to {rect:?}"
            );
        }
    }
}
