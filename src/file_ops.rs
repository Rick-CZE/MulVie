//! Pure file-management logic for List Management's Files mode: batch-rename
//! planning and its two-phase, collision-safe execution (ported from the old
//! standalone rename window), duplicate detection, and "Keep both" naming.
//! Everything here is plain path/set math — unit-testable, no UI.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

// --- Batch rename ------------------------------------------------------------

/// The lowest `count` free numbers (ascending), skipping `used`. The k-th
/// file in list order gets the k-th number.
pub fn assign_numbers(count: usize, used: &HashSet<u32>) -> Vec<u32> {
    let mut out = Vec::with_capacity(count);
    let mut n = 1u32;
    for _ in 0..count {
        while used.contains(&n) {
            n += 1;
        }
        out.push(n);
        n += 1;
    }
    out
}

/// Numbers already taken by files named `<base>_<digits>` in ANY of the
/// subset's folders, excluding the files about to be renamed. Using the union
/// across folders keeps the numbering collision-free everywhere while staying
/// continuous over a subset that spans subfolders. The base matches
/// case-insensitively — Windows filenames are: with existing `AAA_0001` files,
/// a batch renamed to base `aaa` must not think number 1 is free.
pub fn used_numbers(folders: &HashSet<PathBuf>, base: &str, exclude: &HashSet<PathBuf>) -> HashSet<u32> {
    let mut set = HashSet::new();
    let prefix = format!("{}_", base.to_lowercase());
    for folder in folders {
        if let Ok(rd) = std::fs::read_dir(folder) {
            for d in rd.flatten() {
                let path = d.path();
                if exclude.contains(&path) {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if let Some(rest) = stem.strip_prefix(&prefix) {
                    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                        if let Ok(n) = rest.parse::<u32>() {
                            set.insert(n);
                        }
                    }
                }
            }
        }
    }
    set
}

/// Build the (old, new) rename plan for `files` (in list order — the first
/// file becomes `<base>_0001`). Each file is renamed INSIDE its own folder.
/// Entries whose name wouldn't change are dropped.
pub fn plan_renames(
    files: &[PathBuf],
    base: Option<&str>,
    ext: Option<&str>,
) -> Vec<(PathBuf, PathBuf)> {
    if files.is_empty() || (base.is_none() && ext.is_none()) {
        return Vec::new();
    }
    let numbers = if let Some(base) = base {
        let exclude: HashSet<PathBuf> = files.iter().cloned().collect();
        let folders: HashSet<PathBuf> = files
            .iter()
            .filter_map(|p| p.parent().map(|q| q.to_path_buf()))
            .collect();
        let used = used_numbers(&folders, base, &exclude);
        assign_numbers(files.len(), &used)
    } else {
        Vec::new()
    };

    let mut plan = Vec::new();
    for (k, old) in files.iter().enumerate() {
        let new_stem = match base {
            Some(base) => format!("{base}_{:04}", numbers[k]),
            None => old
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
        };
        let new_ext = match ext {
            Some(e) => e.to_string(),
            None => old
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
        };
        let new_name = if new_ext.is_empty() {
            new_stem
        } else {
            format!("{new_stem}.{new_ext}")
        };
        let new_path = old.parent().unwrap_or(Path::new("")).join(&new_name);
        if new_path != *old {
            plan.push((old.clone(), new_path));
        }
    }
    plan
}

/// The outcome counts of an executed rename batch, for the status line.
#[derive(Default)]
pub struct RenameOutcome {
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Temp files whose original could not be restored (surfaced, never lost).
    pub orphans: Vec<PathBuf>,
}

impl RenameOutcome {
    pub fn summary(&self) -> String {
        let mut msg = format!("Renamed {} file(s).", self.done);
        if self.skipped > 0 {
            msg.push_str(&format!(" Skipped {} (name already existed).", self.skipped));
        }
        if self.failed > 0 {
            msg.push_str(&format!(" {} failed.", self.failed));
        }
        if !self.orphans.is_empty() {
            let names: Vec<String> = self.orphans.iter().map(|p| name_of(p)).collect();
            msg.push_str(&format!(
                " Could not restore {} file(s) — recover from: {}.",
                self.orphans.len(),
                names.join(", ")
            ));
        }
        msg
    }
}

/// A rename with a brief bounded retry: a player that just released the file
/// (the app tears its holder down right before a batch) can keep the handle
/// alive for a few more milliseconds — the same lag the delete path covers.
fn rename_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..10 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Err(last.unwrap())
}

/// Two-phase rename: first move every source to a unique temp name in its own
/// folder (so the batch can't collide with its own old names), then
/// temp→target. A target occupied by a file OUTSIDE the batch is skipped and
/// its source restored — nothing is ever clobbered.
pub fn apply_renames(plan: Vec<(PathBuf, PathBuf)>) -> RenameOutcome {
    let mut out = RenameOutcome::default();
    let mut temps: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new(); // (temp, target, original)

    // Phase 1: vacate every source to a FRESH temp name. Never rename onto an
    // existing file — a crashed earlier batch could leave an orphan temp still
    // holding someone's only copy, and rename() replaces its target.
    let mut counter = 0usize;
    for (old, target) in plan.iter() {
        let dir = old.parent().unwrap_or(Path::new("")).to_path_buf();
        let temp = loop {
            let cand = dir.join(format!(".mulvie_rntmp_{counter}"));
            counter += 1;
            if !cand.exists() {
                break cand;
            }
        };
        match rename_retry(old, &temp) {
            Ok(()) => temps.push((temp, target.clone(), old.clone())),
            Err(_) => out.failed += 1,
        }
    }

    // Phase 2: temp → target; on any failure restore the original, and if even
    // that fails remember the temp path so the data is surfaced, not lost.
    for (temp, target, original) in temps {
        if target.exists() {
            if std::fs::rename(&temp, &original).is_err() {
                out.orphans.push(temp);
            }
            out.skipped += 1;
        } else if std::fs::rename(&temp, &target).is_ok() {
            out.done += 1;
        } else {
            if std::fs::rename(&temp, &original).is_err() {
                out.orphans.push(temp);
            }
            out.failed += 1;
        }
    }
    out
}

// --- Duplicate detection -------------------------------------------------------

/// A file's stem with Windows-style copy decorations stripped and case folded:
/// "Picture 158 (1)" → "picture 158", "IMG - Copy" → "img", "foto - kópia" →
/// "foto". Our own "_0001" numbering is deliberately NOT stripped — those are
/// distinct files by design, never duplicate markers.
pub fn normalized_stem(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let mut s = stem.trim().to_lowercase();
    loop {
        let before = s.clone();
        // " (n)" — the Windows duplicate-name suffix.
        if s.ends_with(')') {
            if let Some(open) = s.rfind(" (") {
                let inner = &s[open + 2..s.len() - 1];
                if !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit()) {
                    s.truncate(open);
                }
            }
        }
        // " - Copy" and the Czech/Slovak variants (already lowercased). A
        // numbered form " - copy (2)" loses the "(2)" in the branch above.
        for suf in [" - copy", " - kopie", " - kópia"] {
            if let Some(rest) = s.strip_suffix(suf) {
                s = rest.to_string();
            }
        }
        s = s.trim_end().to_string();
        if s == before {
            return s;
        }
    }
}

/// Groups of possible duplicates WITHIN the same immediate folder: files with
/// the exact same byte size, or the same normalized stem (extension ignored —
/// "picture.jpg" vs "picture.png" counts). Each returned group has ≥2 files,
/// in the same order they appear in `files`; groups follow list order too.
pub fn find_duplicate_groups(files: &[(PathBuf, u64)]) -> Vec<Vec<PathBuf>> {
    use std::collections::HashMap;

    // Union-find over indices: same-folder same-size, or same-folder same-stem.
    let n = files.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        let mut r = i;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = i;
        while parent[c] != r {
            let next = parent[c];
            parent[c] = r;
            c = next;
        }
        r
    }
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[rb.max(ra)] = rb.min(ra);
        }
    };

    let mut by_size: HashMap<(PathBuf, u64), usize> = HashMap::new();
    let mut by_stem: HashMap<(PathBuf, String), usize> = HashMap::new();
    for (i, (path, size)) in files.iter().enumerate() {
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if *size > 0 {
            match by_size.entry((dir.clone(), *size)) {
                std::collections::hash_map::Entry::Occupied(e) => union(&mut parent, *e.get(), i),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }
        let stem = normalized_stem(name);
        if !stem.is_empty() {
            match by_stem.entry((dir, stem)) {
                std::collections::hash_map::Entry::Occupied(e) => union(&mut parent, *e.get(), i),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let g = groups.entry(r).or_default();
        if g.is_empty() {
            order.push(r);
        }
        g.push(files[i].0.clone());
    }
    order
        .into_iter()
        .filter_map(|r| {
            let g = groups.remove(&r)?;
            (g.len() >= 2).then_some(g)
        })
        .collect()
}

// --- Move helpers ---------------------------------------------------------------

/// A free "Keep both" name in `dir` for `name`: "photo.jpg" → "photo (1).jpg",
/// "photo (2).jpg", … (first free number).
pub fn keep_both_name(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str());
    for k in 1u32.. {
        let cand = match ext {
            Some(e) => format!("{stem} ({k}).{e}"),
            None => format!("{stem} ({k})"),
        };
        let cand = dir.join(cand);
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

/// Move one file: plain rename when possible, copy+delete across volumes.
pub fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)?;
            std::fs::remove_file(src)
        }
    }
}

pub fn name_of(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbering fills the lowest free slots, in list order (ported from the
    /// old rename window along with the logic).
    #[test]
    fn numbers_fill_lowest_free_in_order() {
        let used: HashSet<u32> = [1, 3].into_iter().collect();
        assert_eq!(assign_numbers(3, &used), vec![2, 4, 5]);
        assert_eq!(assign_numbers(2, &HashSet::new()), vec![1, 2]);
        let dense: HashSet<u32> = (1..=4).collect();
        assert_eq!(assign_numbers(2, &dense), vec![5, 6]);
    }

    /// Windows copy decorations are stripped (repeatedly, case-insensitively,
    /// incl. Czech/Slovak); our _0001 numbering is NOT.
    #[test]
    fn normalized_stem_strips_copy_suffixes_only() {
        assert_eq!(normalized_stem("picture 158 (1).jpg"), "picture 158");
        assert_eq!(normalized_stem("picture 158.jpg"), "picture 158");
        assert_eq!(normalized_stem("IMG - Copy.png"), "img");
        assert_eq!(normalized_stem("foto - kopie (2).jpg"), "foto");
        assert_eq!(normalized_stem("foto - kópia.png"), "foto");
        // Our rename numbering stays significant.
        assert_eq!(normalized_stem("aaa_0001.jpg"), "aaa_0001");
        assert_ne!(normalized_stem("aaa_0001.jpg"), normalized_stem("aaa_0002.jpg"));
        // "(text)" is not a copy marker.
        assert_eq!(normalized_stem("party (best).jpg"), "party (best)");
    }

    /// Same-folder same-size or same-normalized-stem group together (incl.
    /// cross-extension); different folders never mix; groups need ≥2 files.
    #[test]
    fn duplicate_groups_follow_the_agreed_rules() {
        // Paths built with join() so the separators are native — Windows
        // backslash literals are single-component names on Linux and every
        // "same folder" assertion silently changes meaning.
        let d = |n: &str| Path::new("d").join(n);
        let f = |p: PathBuf, s: u64| (p, s);
        let files = vec![
            f(d("a.jpg"), 100),               // group 1 by size with b.png
            f(d("b.png"), 100),               // …
            f(d("pic.jpg"), 5),               // group 2 by stem with pic.png + pic (1).jpg
            f(d("pic.png"), 6),
            f(d("pic (1).jpg"), 7),
            f(d("unique.gif"), 42),           // alone: no group
            f(Path::new("e").join("a.jpg"), 100), // same size, different folder: alone
            f(d("x_0001.jpg"), 11),           // numbering is not a dupe marker
            f(d("x_0002.jpg"), 12),
        ];
        let groups = find_duplicate_groups(&files);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(groups[0], vec![d("a.jpg"), d("b.png")]);
        assert_eq!(
            groups[1],
            vec![d("pic.jpg"), d("pic.png"), d("pic (1).jpg")]
        );
    }

    /// The taken-numbers scan matches the base case-insensitively, like the
    /// filesystem: existing AAA_0001/AAA_0002 block those numbers for base
    /// "aaa" (otherwise the batch plans onto taken names and no-ops).
    #[test]
    fn used_numbers_ignores_base_case() {
        let dir = std::env::temp_dir().join(format!("mulvie_un_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AAA_0001.jpg"), b"x").unwrap();
        std::fs::write(dir.join("aAa_0002.jpg"), b"x").unwrap();
        std::fs::write(dir.join("other.jpg"), b"x").unwrap();
        let folders: HashSet<PathBuf> = [dir.clone()].into_iter().collect();
        let used = used_numbers(&folders, "aaa", &HashSet::new());
        assert!(used.contains(&1) && used.contains(&2), "{used:?}");
        let used = used_numbers(&folders, "AAA", &HashSet::new());
        assert!(used.contains(&1) && used.contains(&2), "{used:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "Keep both" picks the first free " (n)" name in the destination.
    #[test]
    fn keep_both_finds_the_first_free_number() {
        let dir = std::env::temp_dir().join(format!("mulvie_kb_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No collision yet: (1).
        assert_eq!(keep_both_name(&dir, "photo.jpg"), dir.join("photo (1).jpg"));
        // Occupy (1) and (2): must skip to (3).
        std::fs::write(dir.join("photo (1).jpg"), b"x").unwrap();
        std::fs::write(dir.join("photo (2).jpg"), b"x").unwrap();
        assert_eq!(keep_both_name(&dir, "photo.jpg"), dir.join("photo (3).jpg"));
        // Extensionless name.
        assert_eq!(keep_both_name(&dir, "README"), dir.join("README (1)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// move_file relocates within a volume and leaves nothing behind.
    #[test]
    fn move_file_relocates_and_removes_source() {
        let base = std::env::temp_dir().join(format!("mulvie_mv_{}", std::process::id()));
        let src_dir = base.join("a");
        let dst_dir = base.join("b");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        let src = src_dir.join("f.dat");
        let dst = dst_dir.join("f.dat");
        std::fs::write(&src, b"payload").unwrap();
        move_file(&src, &dst).unwrap();
        assert!(!src.exists(), "source should be gone");
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Rename planning: continuous numbering across a subset spanning
    /// subfolders, each file staying in its own folder.
    #[test]
    fn plan_renames_spans_folders_with_continuous_numbers() {
        // join()-built paths, so the test means the same thing on Linux
        // (backslash literals are one flat component there).
        let root = Path::new("root");
        let sub = root.join("sub");
        let files = vec![root.join("z.jpg"), sub.join("y.png")];
        let plan = plan_renames(&files, Some("AAA"), None);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].1, root.join("AAA_0001.jpg"));
        assert_eq!(plan[1].1, sub.join("AAA_0002.png"));
        // Extension-only change keeps stems and folders.
        let plan = plan_renames(&files, None, Some("jpg"));
        assert_eq!(plan.len(), 1, "z.jpg already has .jpg — only y changes");
        assert_eq!(plan[0].1, sub.join("y.jpg"));
    }
}
