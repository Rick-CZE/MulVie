//! Folder scanning, filtering and sorting for a single pane.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Image extensions MulVie will show (lower-case, no dot). Every one is decoded
/// by the bundled `image` crate with a pure-Rust decoder (see Cargo.toml).
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "tga", "qoi",
];

/// Video extensions MulVie will play (via libmpv). libmpv already carries every
/// codec, so widening this list costs no size and pulls in no dependency.
/// `.ts` is deliberately absent: it doubles as the TypeScript source extension,
/// so a folder of code would otherwise show up as broken black videos.
pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "avi", "mkv", "mov", "webm", "m4v", "flv", "wmv", "mpg", "mpeg", "m2v", "mts", "m2ts",
    "3gp", "3g2", "ogv", "vob", "f4v", "asf", "divx",
];

/// Audio-only extensions MulVie will play, via the *same* libmpv engine as
/// video. A pane playing one of these shows a music note unless the file
/// carries embedded cover art. Playlist formats (`.m3u`/`.pls`) are
/// deliberately excluded: they can reference network URLs, which would break
/// the "nothing leaves the stick" privacy promise.
pub const AUDIO_EXTS: &[&str] = &[
    // Mainstream.
    "mp3", "flac", "wav", "aac", "ogg", "oga", "opus", "m4a", "m4b", "wma", "aiff", "aif", "mka",
    "weba", "3ga",
    // Lossless / less common, all still handled by libmpv/ffmpeg.
    "ape", "wv", "tta", "ac3", "dts", "mp2", "mpc", "spx", "amr", "au", "caf", "dsf", "dff", "w64",
    "ra", "gsm",
];

/// Document extensions MulVie will display (via pdfium).
pub const PDF_EXTS: &[&str] = &["pdf"];

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SortOrder {
    NameAsc,
    NameDesc,
    DateAsc,  // oldest first (file modified time)
    DateDesc, // newest first
    Folder,   // raw filesystem enumeration order
}

impl Default for SortOrder {
    fn default() -> Self {
        SortOrder::NameAsc
    }
}

impl SortOrder {
    pub const ALL: [SortOrder; 5] = [
        SortOrder::NameAsc,
        SortOrder::NameDesc,
        SortOrder::DateAsc,
        SortOrder::DateDesc,
        SortOrder::Folder,
    ];

    // Labels must stay ASCII: the bundled UI font has no glyph for arrows
    // like `→`, which render as empty boxes.
    pub fn label(self) -> &'static str {
        match self {
            SortOrder::NameAsc => "Name  A to Z",
            SortOrder::NameDesc => "Name  Z to A",
            SortOrder::DateAsc => "Date  old to new",
            SortOrder::DateDesc => "Date  new to old",
            SortOrder::Folder => "Folder order",
        }
    }
}

/// Every extension MulVie supports, lower-case — the "everything included"
/// state of Gallery management's file-type filter.
pub fn all_supported_exts() -> std::collections::HashSet<String> {
    IMAGE_EXTS
        .iter()
        .chain(VIDEO_EXTS)
        .chain(AUDIO_EXTS)
        .chain(PDF_EXTS)
        .map(|e| e.to_string())
        .collect()
}

fn ext_in(path: &Path, list: &[&str]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => list.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// True if `path` is an image MulVie can decode.
pub fn is_image(path: &Path) -> bool {
    ext_in(path, IMAGE_EXTS)
}

/// True if `path` is a video MulVie can play.
pub fn is_video(path: &Path) -> bool {
    ext_in(path, VIDEO_EXTS)
}

/// True if `path` is an audio-only file MulVie can play.
pub fn is_audio(path: &Path) -> bool {
    ext_in(path, AUDIO_EXTS)
}

/// True if `path` is played through the mpv engine — a video file or an
/// audio-only file. Both are backed by the same `VideoPlayer`; audio-only panes
/// simply render a music note instead of a video frame.
pub fn is_playable(path: &Path) -> bool {
    is_video(path) || is_audio(path)
}

/// True if `path` is a PDF MulVie can display.
pub fn is_pdf(path: &Path) -> bool {
    ext_in(path, PDF_EXTS)
}

/// True if `path` is any media MulVie shows (image, video, audio, or PDF).
pub fn is_media(path: &Path) -> bool {
    is_image(path) || is_playable(path) || is_pdf(path)
}

struct Entry {
    path: PathBuf,
    modified: SystemTime,
}

fn file_name_str(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Defensive cap on subfolder nesting (far beyond any real media library).
const MAX_DEPTH: u32 = 32;

/// Scan a folder AND its whole subtree for media, depth-first: the folder's
/// own files first (in `order`), then each subfolder's subtree, subfolders in
/// natural name order. Sorting therefore re-orders files WITHIN their folder;
/// the folder grouping itself is fixed. Directory symlinks/junctions are
/// skipped, which also keeps the walk cycle-free.
pub fn scan_folder(folder: &Path, order: SortOrder) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_into(folder, order, 0, &mut out);
    out
}

fn scan_into(folder: &Path, order: SortOrder, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(folder) {
        for dent in read_dir.flatten() {
            let path = dent.path();
            // DirEntry::file_type does NOT follow symlinks: a junction or a
            // symlinked directory reports is_symlink (not is_dir), so linked
            // directories are naturally excluded — no infinite loops.
            let Ok(ft) = dent.file_type() else { continue };
            if ft.is_dir() {
                dirs.push(path);
                continue;
            }
            if !ft.is_file() || !is_media(&path) {
                continue;
            }
            let modified = dent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(Entry { path, modified });
        }
    }
    sort_entries(&mut entries, order);
    out.extend(entries.into_iter().map(|e| e.path));
    dirs.sort_by(|a, b| natord::compare(&file_name_str(a), &file_name_str(b)));
    for d in dirs {
        scan_into(&d, order, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The bundled UI font renders glyphs like `→` as empty boxes, so the
    /// sort-menu labels must stay plain ASCII.
    #[test]
    fn sort_labels_are_ascii_only() {
        for order in SortOrder::ALL {
            assert!(
                order.label().is_ascii(),
                "non-ASCII glyph in {:?}",
                order.label()
            );
        }
    }

    #[test]
    fn classifies_each_category() {
        assert!(is_image(Path::new("a.png")) && is_image(Path::new("b.TGA")) && is_image(Path::new("c.qoi")));
        assert!(is_video(Path::new("a.wmv")) && is_video(Path::new("b.MPEG")) && is_video(Path::new("c.m2ts")));
        assert!(is_audio(Path::new("a.mp3")) && is_audio(Path::new("b.FLAC")) && is_audio(Path::new("c.opus")));
        assert!(is_pdf(Path::new("a.pdf")));
        // Audio is played through the mpv engine, like video.
        assert!(is_playable(Path::new("song.mp3")) && is_playable(Path::new("clip.mp4")));
        assert!(!is_playable(Path::new("photo.png")));
        // Audio counts as media so folders list it and drops accept it.
        assert!(is_media(Path::new("song.flac")));
        // A category must not misclaim another's file.
        assert!(!is_video(Path::new("song.mp3")) && !is_audio(Path::new("clip.mp4")));
    }

    /// No extension may belong to two categories at once. This guards the
    /// deliberately close pairs (ogg/ogv, m4a/m4v, mka/mkv) from ever drifting
    /// into a collision, which would route a file to the wrong renderer.
    #[test]
    fn extension_sets_are_disjoint() {
        let sets = [
            ("image", IMAGE_EXTS),
            ("video", VIDEO_EXTS),
            ("audio", AUDIO_EXTS),
            ("pdf", PDF_EXTS),
        ];
        for (i, (na, a)) in sets.iter().enumerate() {
            for (nb, b) in sets.iter().skip(i + 1) {
                for ext in *a {
                    assert!(
                        !b.contains(ext),
                        "extension {ext:?} is in both {na} and {nb} sets"
                    );
                }
            }
        }
    }

    /// The recursive scan lists the root's own files first (sorted), then
    /// each subfolder's subtree with subfolders in natural name order.
    #[test]
    fn scan_lists_root_files_then_subtrees_in_name_order() {
        let root = std::env::temp_dir().join(format!("mulvie_scan_{}", std::process::id()));
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let b = mk("b.png");
        let a = mk("a.png");
        let d = mk("sub1/d.png");
        let e = mk("sub1/deeper/e.png");
        let c = mk("sub2/c.png");
        mk("sub1/notes.txt"); // unsupported: ignored

        let got = scan_folder(&root, SortOrder::NameAsc);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got, vec![a, b, d, e, c]);
    }

    /// Every extension is stored lower-case (matching gets ASCII-lowered).
    #[test]
    fn extensions_are_lowercase() {
        for set in [IMAGE_EXTS, VIDEO_EXTS, AUDIO_EXTS, PDF_EXTS] {
            for ext in set {
                assert_eq!(*ext, ext.to_ascii_lowercase(), "non-lowercase ext {ext:?}");
            }
        }
    }
}

fn sort_entries(entries: &mut [Entry], order: SortOrder) {
    match order {
        SortOrder::NameAsc => {
            entries.sort_by(|a, b| natord::compare(&file_name_str(&a.path), &file_name_str(&b.path)))
        }
        SortOrder::NameDesc => {
            entries.sort_by(|a, b| natord::compare(&file_name_str(&b.path), &file_name_str(&a.path)))
        }
        SortOrder::DateAsc => entries.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| natord::compare(&file_name_str(&a.path), &file_name_str(&b.path)))
        }),
        SortOrder::DateDesc => entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| natord::compare(&file_name_str(&a.path), &file_name_str(&b.path)))
        }),
        SortOrder::Folder => { /* keep read_dir order */ }
    }
}
