//! Widen glyph coverage by adding the host's system fonts as fallbacks, so
//! filenames and labels in other scripts (Japanese, Korean, Chinese, Cyrillic,
//! Greek, Arabic, …) and assorted symbols render instead of showing empty
//! boxes. egui's built-in font stays PRIMARY, so ordinary Latin text keeps the
//! app's look; the system fonts only fill glyphs it lacks.
//!
//! Robustness (the explicit requirement): every candidate is optional and is
//! validated before use. A font file that is missing, or that doesn't parse as
//! a recognised font, is skipped — it can never crash the app. Worst case, a
//! glyph nothing covers shows as a box, exactly as before.

use eframe::egui::{self, FontFamily};

/// One fallback "slot" per script/coverage area. Within a slot the paths are
/// tried in order and the first that loads wins, so we pull in at most one font
/// per script (keeping memory down) while still coping if a machine ships an
/// alternative face. Windows ships these on a typical install; the East-Asian
/// ones may be absent on a stripped-down machine, which is fine — skipped.
#[cfg(windows)]
const SLOTS: &[(&str, &[&str])] = &[
    // Broad Latin/Cyrillic/Greek/Armenian/Hebrew/Arabic/Thai/… — small, high value.
    ("sys_broad", &[r"C:\Windows\Fonts\segoeui.ttf"]),
    // A very large range of symbol blocks (arrows, maths, dingbats, geometric…).
    ("sys_symbols", &[r"C:\Windows\Fonts\seguisym.ttf"]),
    // Japanese (also covers most shared Han), Korean, then Simplified Chinese.
    (
        "sys_japanese",
        &[r"C:\Windows\Fonts\YuGothR.ttc", r"C:\Windows\Fonts\msgothic.ttc"],
    ),
    ("sys_korean", &[r"C:\Windows\Fonts\malgun.ttf"]),
    (
        "sys_chinese",
        &[r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simsun.ttc"],
    ),
];

/// Linux (paths as shipped on Mint/Ubuntu; every entry optional): DejaVu for
/// broad Latin/Cyrillic/Greek + a large symbol range, Noto CJK for
/// Japanese/Korean/Chinese (one .ttc covers all three), Noto Symbols2 for the
/// remaining symbol blocks where present.
#[cfg(target_os = "linux")]
const SLOTS: &[(&str, &[&str])] = &[
    (
        "sys_broad",
        &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"],
    ),
    (
        "sys_cjk",
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        ],
    ),
    (
        "sys_symbols",
        &["/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf"],
    ),
];

#[cfg(not(any(windows, target_os = "linux")))]
const SLOTS: &[(&str, &[&str])] = &[];

/// Load whatever system fonts are available and append them to egui's fallback
/// chains. Safe to call once at startup; also covers child viewports (the
/// rename window), which share this context's fonts.
pub fn install_system_fallbacks(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();

    for (name, paths) in SLOTS {
        for path in *paths {
            let Ok(bytes) = std::fs::read(path) else {
                continue; // not installed — try the next candidate in this slot
            };
            // Only trust it if it parses as a real font (index 0 also handles
            // `.ttc` collections). A bad file is skipped, never fatal.
            if ab_glyph::FontRef::try_from_slice_and_index(&bytes, 0).is_err() {
                continue;
            }
            fonts
                .font_data
                .insert((*name).to_owned(), egui::FontData::from_owned(bytes));
            added.push((*name).to_owned());
            break; // one font per slot is enough
        }
    }

    // Append after egui's own fonts so Latin text keeps the built-in look and
    // these only supply glyphs it can't.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.extend(added.iter().cloned());
        }
    }

    ctx.set_fonts(fonts);
}
