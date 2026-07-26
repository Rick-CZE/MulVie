//! Persisted app settings.
//!
//! The config lives *next to the executable* — i.e. on the USB stick, never on
//! the host PC. If the location is read-only the save is silently skipped so
//! MulVie still runs from write-protected media.
//!
//! Deliberately NOT persisted (privacy + fresh-start behavior): which folders
//! and files the panes were showing, and the pane layout. Those live only for
//! the running session; every launch starts as a clean single-panel gallery.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub maximized: bool,
    /// Global "loop folder" toggle: wrap from the last item back to the first.
    /// Off by default; `#[serde(default)]` keeps older config files loading.
    #[serde(default)]
    pub loop_enabled: bool,
    /// Auto-hide the cursor when it rests over content. ON by default (the
    /// button is only highlighted when the user turns it OFF).
    #[serde(default = "default_true")]
    pub mouse_hide: bool,
    /// Background tint colour (RGB) shown behind content and in empty panels.
    /// Defaults to the app's deep navy.
    #[serde(default = "default_bg_color")]
    pub bg_color: [u8; 3],
    /// Background tint opacity (0 = fully see-through frosted glass, 255 =
    /// solid colour). Only has a visible see-through effect where Windows
    /// acrylic blur is available; otherwise the colour is drawn solid.
    #[serde(default = "default_bg_alpha")]
    pub bg_alpha: u8,
    /// Colour of the file-name text in Gallery management (list + grid). Only
    /// the item names follow this; chrome and other text keep the theme.
    #[serde(default = "default_text_color")]
    pub text_color: [u8; 3],
    /// Autoplay: when ON (default), a video/audio starts playing as soon as it
    /// becomes the current file. OFF makes every one start paused (GIFs are
    /// unaffected). Persisted.
    #[serde(default = "default_true")]
    pub autoplay: bool,
    /// Sound on startup: OFF (default) launches with every panel muted; ON
    /// launches unmuted. Persisted.
    #[serde(default)]
    pub sound_on_startup: bool,
    /// Four-panel default: OFF (default) launches as a single panel; ON opens
    /// the four-panel view when launched EMPTY (a file passed on the command
    /// line still opens single, with that file). Persisted.
    #[serde(default)]
    pub four_panel_default: bool,
    /// Presentation cover freezes content: when ON, raising the cover also
    /// pauses every panel (incl. locked); lifting it resumes what was playing.
    /// OFF by default. Persisted.
    #[serde(default)]
    pub cover_freezes: bool,
}

fn default_true() -> bool {
    true
}

/// The app's default background navy (matches `theme::CANVAS`).
pub fn default_bg_color() -> [u8; 3] {
    [0x0D, 0x14, 0x20]
}

/// The default background opacity — the frosted navy the app has always shipped.
pub fn default_bg_alpha() -> u8 {
    0xA6
}

/// The default Gallery-management name colour (matches `theme::SILVER`).
pub fn default_text_color() -> [u8; 3] {
    [0xC8, 0xCF, 0xDA]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window_size: None,
            maximized: false,
            loop_enabled: false,
            mouse_hide: true,
            bg_color: default_bg_color(),
            bg_alpha: default_bg_alpha(),
            text_color: default_text_color(),
            autoplay: true,
            sound_on_startup: false,
            four_panel_default: false,
            cover_freezes: false,
        }
    }
}

fn exe_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

fn config_path() -> Option<PathBuf> {
    Some(exe_dir()?.join("mulvie_config.json"))
}

/// The "open this file" hand-over inbox used by a second MulVie instance
/// launched via file association while one is already running. Lives next to
/// the exe (on the stick), written by the new instance, consumed and deleted
/// by the running one.
pub fn inbox_path() -> Option<PathBuf> {
    Some(exe_dir()?.join("mulvie_open.txt"))
}

/// Atomically hand a file path to the running instance: write to a temp file
/// then rename into place, so the reader never sees a half-written inbox.
/// Returns false if the write failed (e.g. read-only media) — the caller then
/// opens its own window instead of exiting with nothing shown.
pub fn write_inbox(path: &Path) -> bool {
    let Some(inbox) = inbox_path() else {
        return false;
    };
    let tmp = inbox.with_extension("txt.tmp");
    if std::fs::write(&tmp, path.to_string_lossy().as_bytes()).is_err() {
        return false;
    }
    std::fs::rename(&tmp, &inbox).is_ok()
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) {
    let Some(path) = config_path() else { return };
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        // Best-effort: ignore failures (read-only stick, locked file, etc.).
        let _ = std::fs::write(&path, text);
    }
}

// --- Libraries -------------------------------------------------------------
//
// A "library" is a user-saved snapshot of the panel layout: what each panel
// was showing (a file+folder, an empty panel, or a pinned Gallery management
// on a folder), the relative pane sizes, and which panels were muted. Only
// PATHS are stored — never any content — in the app's own file next to the exe
// (on the stick), so nothing is written to the host. This is the one opt-in
// exception to "nothing persists across restarts": only libraries the user
// explicitly saves are remembered.

/// What a single panel was showing when a library was saved.
#[derive(Clone, Serialize, Deserialize)]
pub enum PanelContent {
    Empty,
    /// A browsed folder with a current file.
    Content { folder: PathBuf, file: PathBuf },
    /// A pinned Gallery management browsing this folder (None = empty panel).
    Gallery { folder: Option<PathBuf> },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PanelSnapshot {
    pub content: PanelContent,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Library {
    pub name: String,
    /// Relative pane sizes: the layout's (vertical, left-h, right-h) fractions.
    pub layout: [f32; 3],
    pub panels: [PanelSnapshot; 4],
}

fn libraries_path() -> Option<PathBuf> {
    Some(exe_dir()?.join("mulvie_libraries.json"))
}

pub fn load_libraries() -> Vec<Library> {
    let Some(path) = libraries_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(libs) => libs,
            Err(_) => {
                // The file exists but doesn't parse (torn write from a yanked
                // stick, hand-edit, …). QUARANTINE it instead of silently
                // treating it as empty: an empty in-memory list would rewrite
                // the file on the next save and permanently destroy every
                // library, while the corrupt text is usually hand-recoverable.
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Save the whole library list. ATOMIC (temp file + rename, same pattern as
/// [`write_inbox`]) so a stick yanked mid-save can never leave a truncated
/// file — the libraries are the user's only deliberately-persisted data.
/// Returns whether the save actually reached the disk, so the UI can say so.
pub fn save_libraries(libs: &[Library]) -> bool {
    let Some(path) = libraries_path() else {
        return false;
    };
    let Ok(text) = serde_json::to_string_pretty(libs) else {
        return false;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_err() {
        return false; // read-only stick, …
    }
    std::fs::rename(&tmp, &path).is_ok()
}
