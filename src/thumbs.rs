//! RAM-only thumbnails for the List-Management browser.
//!
//! MulVie renders every thumbnail itself — never through the Windows shell —
//! so nothing about the user's pictures ever reaches Explorer's on-disk
//! thumbnail cache. Decoding happens on a dedicated worker thread (separate
//! from the full-size decoder so browsing never delays the main viewer);
//! the UI thread uploads results as GPU textures into an LRU cache bounded
//! by a byte budget. Nothing is written to disk; closing the app frees it all.
//!
//! Thumbnails come in a few fixed resolution TIERS. The icon-size slider maps
//! to the smallest tier that still looks sharp, so a slider drag re-uses
//! cached tiers instead of re-decoding on every pixel of movement.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};

/// Fixed thumbnail resolutions (longest edge, px).
pub const TIERS: [u32; 4] = [96, 192, 384, 768];

/// Soft memory ceiling for cached thumbnails (~a few hundred big tiles).
const BUDGET_BYTES: usize = 384 * 1024 * 1024;

/// Pending-decode ceiling. Newest requests decode FIRST (what's on screen was
/// requested this frame); beyond the cap the OLDEST queued request is dropped
/// and un-marked inflight — if its tile scrolls back into view it simply
/// re-requests. Without this, fast-scrolling a big tree left the visible rows
/// waiting behind minutes of stale FIFO backlog.
const QUEUE_CAP: usize = 128;

/// The smallest tier that still looks sharp at `icon_px` on-screen pixels
/// (physical). Beyond the largest tier the tile upscales slightly.
pub fn tier_for(icon_physical_px: f32) -> u32 {
    for t in TIERS {
        if t as f32 >= icon_physical_px {
            return t;
        }
    }
    *TIERS.last().unwrap()
}

pub struct Thumb {
    pub tex: TextureHandle,
    /// Source pixel size of the thumbnail (aspect = the original's).
    pub size: [usize; 2],
    bytes: usize,
}

enum Decoded {
    Ok {
        key: (PathBuf, u32),
        image: ColorImage,
    },
    Err {
        key: (PathBuf, u32),
    },
}

/// The request queue shared with the worker: a stack (newest decodes first) so
/// the tiles actually on screen never wait behind a scrolled-past backlog.
struct ReqQueue {
    queue: Mutex<(VecDeque<(PathBuf, u32)>, bool)>, // (pending, shutdown)
    cv: Condvar,
}

pub struct ThumbStore {
    req: Arc<ReqQueue>,
    res_rx: Receiver<Decoded>,
    cache: HashMap<(PathBuf, u32), Rc<Thumb>>,
    lru: VecDeque<(PathBuf, u32)>,
    inflight: HashSet<(PathBuf, u32)>,
    /// Failures are per PATH: a file that won't decode at one size won't
    /// decode at another, so don't retry it per tier.
    failed: HashSet<PathBuf>,
    /// Keys invalidated while their decode was already running: the stale
    /// result must be discarded when it arrives, not cached.
    dropped: HashSet<(PathBuf, u32)>,
    used_bytes: usize,
}

impl ThumbStore {
    pub fn new() -> Self {
        let req = Arc::new(ReqQueue {
            queue: Mutex::new((VecDeque::new(), false)),
            cv: Condvar::new(),
        });
        let (res_tx, res_rx) = channel::<Decoded>();

        let worker_req = Arc::clone(&req);
        std::thread::Builder::new()
            .name("mulvie-thumbs".into())
            .spawn(move || loop {
                let key = {
                    let mut guard = worker_req.queue.lock().unwrap();
                    loop {
                        if guard.1 {
                            return; // shutdown
                        }
                        // Newest first: the back of the queue is what the UI
                        // asked for most recently — i.e. what's on screen.
                        if let Some(key) = guard.0.pop_back() {
                            break key;
                        }
                        guard = worker_req.cv.wait(guard).unwrap();
                    }
                };
                // A malformed file must not kill the worker (a dead worker
                // would leave every pending tile "Loading" forever).
                let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_thumb(&key.0, key.1)
                }))
                .unwrap_or(None);
                let msg = match decoded {
                    Some(image) => Decoded::Ok { key, image },
                    None => Decoded::Err { key },
                };
                if res_tx.send(msg).is_err() {
                    return; // UI gone
                }
            })
            .expect("spawn thumbnail thread");

        Self {
            req,
            res_rx,
            cache: HashMap::new(),
            lru: VecDeque::new(),
            inflight: HashSet::new(),
            failed: HashSet::new(),
            dropped: HashSet::new(),
            used_bytes: 0,
        }
    }

    /// Queue an IMAGE thumbnail decode (PDF thumbs go through `insert` — the
    /// pdfium binding is not thread-safe, so those render on the UI thread).
    pub fn request(&mut self, path: &Path, tier: u32) {
        let key = (path.to_path_buf(), tier);
        if self.cache.contains_key(&key) || self.inflight.contains(&key) || self.failed.contains(path)
        {
            return;
        }
        self.inflight.insert(key.clone());
        let mut guard = self.req.queue.lock().unwrap();
        guard.0.push_back(key);
        // Over the cap, drop the OLDEST request (scrolled past long ago); its
        // inflight mark goes too so a re-appearing tile can re-request it.
        while guard.0.len() > QUEUE_CAP {
            if let Some(old) = guard.0.pop_front() {
                self.inflight.remove(&old);
            }
        }
        drop(guard);
        self.req.cv.notify_one();
    }

    /// The thumbnail for `path` at exactly `tier`, if cached.
    pub fn get(&mut self, path: &Path, tier: u32) -> Option<Rc<Thumb>> {
        let key = (path.to_path_buf(), tier);
        let hit = self.cache.get(&key).cloned();
        if hit.is_some() {
            self.touch(&key);
        }
        hit
    }

    /// The best cached thumbnail at ANY tier (largest first) — lets a tile
    /// show a slightly-soft stand-in while its proper tier decodes.
    pub fn get_any(&mut self, path: &Path) -> Option<Rc<Thumb>> {
        for t in TIERS.iter().rev() {
            let key = (path.to_path_buf(), *t);
            if let Some(hit) = self.cache.get(&key).cloned() {
                self.touch(&key);
                return Some(hit);
            }
        }
        None
    }

    pub fn is_failed(&self, path: &Path) -> bool {
        self.failed.contains(path)
    }

    /// Store a UI-thread-rendered thumbnail (PDF page minis). `None` marks the
    /// file as failed so it isn't retried every frame.
    pub fn insert(&mut self, ctx: &Context, path: &Path, tier: u32, image: Option<ColorImage>) {
        match image {
            Some(img) => self.upload(ctx, (path.to_path_buf(), tier), img),
            None => {
                self.failed.insert(path.to_path_buf());
            }
        }
    }

    /// Drain finished decodes into GPU textures. Call once per frame from the
    /// UI thread; returns true if anything new arrived (repaint-worthy).
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                Decoded::Ok { key, image } => {
                    self.inflight.remove(&key);
                    if self.dropped.remove(&key) {
                        continue; // invalidated mid-decode: stale content
                    }
                    self.upload(ctx, key, image);
                    changed = true;
                }
                Decoded::Err { key } => {
                    self.inflight.remove(&key);
                    if self.dropped.remove(&key) {
                        continue;
                    }
                    self.failed.insert(key.0);
                }
            }
        }
        changed
    }

    /// Forget everything cached for `path` — its content changed on disk
    /// (move-replace, rename reusing the name). Queued decodes are dropped;
    /// one already running is discarded when it lands.
    pub fn invalidate(&mut self, path: &Path) {
        self.failed.remove(path);
        let stale: Vec<(PathBuf, u32)> = self
            .cache
            .keys()
            .filter(|k| k.0 == *path)
            .cloned()
            .collect();
        for key in stale {
            if let Some(old) = self.cache.remove(&key) {
                self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
            }
            if let Some(pos) = self.lru.iter().position(|k| *k == key) {
                self.lru.remove(pos);
            }
        }
        let mut guard = self.req.queue.lock().unwrap();
        guard.0.retain(|k| {
            let keep = k.0 != *path;
            if !keep {
                self.inflight.remove(k);
            }
            keep
        });
        drop(guard);
        // Whatever is STILL inflight for this path is being decoded right now.
        let running: Vec<(PathBuf, u32)> = self
            .inflight
            .iter()
            .filter(|k| k.0 == *path)
            .cloned()
            .collect();
        self.dropped.extend(running);
    }

    /// Retry everything that ever failed (the Refresh button: a file that was
    /// mid-copy when first thumbed deserves a second chance).
    pub fn clear_failed(&mut self) {
        self.failed.clear();
    }

    fn upload(&mut self, ctx: &Context, key: (PathBuf, u32), image: ColorImage) {
        let size = [image.width(), image.height()];
        let bytes = size[0] * size[1] * 4;
        let name = format!("thumb:{}@{}", key.0.to_string_lossy(), key.1);
        let tex = ctx.load_texture(name, image, TextureOptions::LINEAR);
        if let Some(old) = self.cache.insert(key.clone(), Rc::new(Thumb { tex, size, bytes })) {
            self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
        }
        self.used_bytes += bytes;
        self.touch(&key);
        self.evict();
    }

    fn touch(&mut self, key: &(PathBuf, u32)) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
    }

    fn evict(&mut self) {
        while self.used_bytes > BUDGET_BYTES && self.lru.len() > 1 {
            let Some(victim) = self.lru.pop_front() else {
                break;
            };
            if let Some(old) = self.cache.remove(&victim) {
                self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
            }
        }
    }
}

impl Drop for ThumbStore {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.req.queue.lock() {
            guard.1 = true; // tell the worker to exit its wait loop
        }
        self.req.cv.notify_one();
    }
}

// --- Worker-thread decoding ------------------------------------------------

/// Decode `path` and shrink it so its longest edge is `tier` px. Content-based
/// format detection, like the main viewer (a `.png` holding JPEG bytes still
/// decodes). A GIF yields its first frame.
fn decode_thumb(path: &Path, tier: u32) -> Option<ColorImage> {
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let thumb = img.thumbnail(tier, tier);
    let rgba = thumb.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    Some(ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::{tier_for, TIERS};

    /// The slider maps to the smallest tier that stays sharp; huge tiles
    /// clamp to the largest tier (mild upscaling is accepted).
    #[test]
    fn tier_picks_smallest_sharp_tier() {
        assert_eq!(tier_for(24.0), 96);
        assert_eq!(tier_for(96.0), 96);
        assert_eq!(tier_for(97.0), 192);
        assert_eq!(tier_for(300.0), 384);
        assert_eq!(tier_for(500.0), 768);
        assert_eq!(tier_for(2000.0), *TIERS.last().unwrap());
    }

    /// Tiers must be ascending and unique (get_any relies on the order).
    #[test]
    fn tiers_are_ascending() {
        for w in TIERS.windows(2) {
            assert!(w[0] < w[1]);
        }
    }
}
