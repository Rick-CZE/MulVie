//! Background image loading, GPU-texture caching and preloading.
//!
//! Decoding happens on a worker thread; the main (UI) thread uploads the
//! decoded pixels to GPU textures and keeps an LRU cache bounded by a byte
//! budget. Animated GIFs are decoded to a vector of frames with per-frame
//! delays.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};

/// Largest edge (px) we keep for a decoded frame. Bigger images are downscaled
/// on the worker thread — this keeps GPU upload within texture-size limits and
/// caps memory, while still leaving plenty of pixels to zoom into.
const MAX_EDGE: u32 = 8192;

/// Soft memory ceiling for cached, decoded imagery.
const BUDGET_BYTES: usize = 3 * 1024 * 1024 * 1024;

pub struct Frame {
    pub tex: TextureHandle,
    /// Seconds to display this frame (0 for a static image).
    pub delay: f32,
}

pub struct LoadedImage {
    pub frames: Vec<Frame>,
    pub size: [usize; 2],
    pub bytes: usize,
}

impl LoadedImage {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }
}

/// Message from worker -> UI thread.
enum Decoded {
    Ok {
        path: PathBuf,
        frames: Vec<(ColorImage, f32)>,
        size: [usize; 2],
        bytes: usize,
    },
    Err {
        path: PathBuf,
        msg: String,
    },
}

pub struct ImageStore {
    req_tx: Sender<PathBuf>,
    res_rx: Receiver<Decoded>,
    cache: HashMap<PathBuf, Rc<LoadedImage>>,
    lru: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    failed: HashSet<PathBuf>,
    /// Paths invalidated while their decode was already running: the stale
    /// result is discarded when it arrives.
    dropped: HashSet<PathBuf>,
    used_bytes: usize,
}

impl ImageStore {
    pub fn new() -> Self {
        let (req_tx, req_rx) = channel::<PathBuf>();
        let (res_tx, res_rx) = channel::<Decoded>();

        std::thread::Builder::new()
            .name("mulvie-decoder".into())
            .spawn(move || {
                while let Ok(path) = req_rx.recv() {
                    let decoded = decode(&path);
                    if res_tx.send(decoded).is_err() {
                        break; // UI gone
                    }
                }
            })
            .expect("spawn decoder thread");

        Self {
            req_tx,
            res_rx,
            cache: HashMap::new(),
            lru: VecDeque::new(),
            inflight: HashSet::new(),
            failed: HashSet::new(),
            dropped: HashSet::new(),
            used_bytes: 0,
        }
    }

    /// Forget everything cached for `path` — its content changed on disk
    /// (move-replace, rename reusing the name). The next `request` re-decodes.
    pub fn invalidate(&mut self, path: &Path) {
        self.failed.remove(path);
        if let Some(old) = self.cache.remove(path) {
            self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
        }
        if let Some(pos) = self.lru.iter().position(|p| p == path) {
            self.lru.remove(pos);
        }
        if self.inflight.contains(path) {
            self.dropped.insert(path.to_path_buf());
        }
    }

    /// Ask the worker to decode `path` if we don't already have it.
    pub fn request(&mut self, path: &Path) {
        if path.as_os_str().is_empty()
            || self.cache.contains_key(path)
            || self.inflight.contains(path)
            || self.failed.contains(path)
        {
            return;
        }
        self.inflight.insert(path.to_path_buf());
        let _ = self.req_tx.send(path.to_path_buf());
    }

    /// Retrieve a decoded image, marking it most-recently-used.
    pub fn get(&mut self, path: &Path) -> Option<Rc<LoadedImage>> {
        if let Some(img) = self.cache.get(path).cloned() {
            self.touch(path);
            Some(img)
        } else {
            None
        }
    }

    pub fn is_failed(&self, path: &Path) -> bool {
        self.failed.contains(path)
    }

    fn touch(&mut self, path: &Path) {
        if let Some(pos) = self.lru.iter().position(|p| p == path) {
            self.lru.remove(pos);
        }
        self.lru.push_back(path.to_path_buf());
    }

    /// Drain finished decodes and upload them to GPU textures. Call once per
    /// frame on the UI thread. Returns true if anything new arrived.
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                Decoded::Ok {
                    path,
                    frames,
                    size,
                    bytes,
                } => {
                    self.inflight.remove(&path);
                    if self.dropped.remove(&path) {
                        continue; // invalidated mid-decode: stale content
                    }
                    let mut handles = Vec::with_capacity(frames.len());
                    for (i, (img, delay)) in frames.into_iter().enumerate() {
                        let name = format!("{}#{i}", path.to_string_lossy());
                        let tex = ctx.load_texture(name, img, TextureOptions::LINEAR);
                        handles.push(Frame { tex, delay });
                    }
                    let loaded = Rc::new(LoadedImage {
                        frames: handles,
                        size,
                        bytes,
                    });
                    if let Some(old) = self.cache.insert(path.clone(), loaded) {
                        self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
                    }
                    self.used_bytes += bytes;
                    self.touch(&path);
                    self.evict_except(&path);
                    changed = true;
                }
                Decoded::Err { path, msg } => {
                    self.inflight.remove(&path);
                    if self.dropped.remove(&path) {
                        continue;
                    }
                    self.failed.insert(path.clone());
                    eprintln!("MulVie: failed to load {}: {msg}", path.display());
                }
            }
        }
        changed
    }

    fn evict_except(&mut self, keep: &Path) {
        while self.used_bytes > BUDGET_BYTES && self.lru.len() > 1 {
            let Some(victim) = self.lru.iter().find(|p| p.as_path() != keep).cloned() else {
                break;
            };
            if let Some(pos) = self.lru.iter().position(|p| *p == victim) {
                self.lru.remove(pos);
            }
            if let Some(old) = self.cache.remove(&victim) {
                self.used_bytes = self.used_bytes.saturating_sub(old.bytes);
                // A pane may still hold an Rc to `old`; its textures are freed
                // once that pane navigates away. Memory stays bounded by the
                // number of visible panes.
            }
        }
    }
}

// --- Worker-thread decoding ----------------------------------------------

fn decode(path: &Path) -> Decoded {
    let is_gif = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gif"))
        .unwrap_or(false);

    let result = if is_gif {
        // A ".gif" that isn't really a GIF (mislabeled) falls back to the
        // content-sniffing static decoder rather than failing outright.
        decode_gif(path).or_else(|_| decode_static(path))
    } else {
        decode_static(path)
    };

    match result {
        Ok((frames, size)) => {
            let bytes = frames
                .iter()
                .map(|(img, _)| img.width() * img.height() * 4)
                .sum();
            Decoded::Ok {
                path: path.to_path_buf(),
                frames,
                size,
                bytes,
            }
        }
        Err(msg) => Decoded::Err {
            path: path.to_path_buf(),
            msg,
        },
    }
}

fn downscale(img: image::DynamicImage) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    if longest <= MAX_EDGE {
        return img;
    }
    let scale = MAX_EDGE as f32 / longest as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    img.resize(nw, nh, image::imageops::FilterType::Triangle)
}

fn decode_static(path: &Path) -> Result<(Vec<(ColorImage, f32)>, [usize; 2]), String> {
    // Detect the format from the file's CONTENTS, not its extension. A file
    // named ".png" may actually contain JPEG (or vice-versa) — common with
    // renamed/mislabeled files — and the extension-based `image::open` would
    // hand it to the wrong decoder and fail. `with_guessed_format` reads the
    // magic bytes and picks the right one, so MulVie opens whatever the file
    // truly is, like other viewers do.
    let img = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let img = downscale(img);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let color = ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
    Ok((vec![(color, 0.0)], [w, h]))
}

fn decode_gif(path: &Path) -> Result<(Vec<(ColorImage, f32)>, [usize; 2]), String> {
    use image::AnimationDecoder;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let reader = std::io::BufReader::new(file);
    let decoder = image::codecs::gif::GifDecoder::new(reader).map_err(|e| e.to_string())?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| e.to_string())?;
    if frames.is_empty() {
        return Err("GIF contained no frames".into());
    }

    let mut out = Vec::with_capacity(frames.len());
    let mut size = [0usize, 0usize];
    for frame in frames {
        let (num, den) = frame.delay().numer_denom_ms();
        let ms = if den == 0 { 100.0 } else { num as f32 / den as f32 };
        let delay = (ms / 1000.0).clamp(0.02, 10.0);
        let buffer = frame.into_buffer();
        let (w, h) = (buffer.width() as usize, buffer.height() as usize);
        size = [w, h];
        let color = ColorImage::from_rgba_unmultiplied([w, h], buffer.as_raw());
        out.push((color, delay));
    }
    Ok((out, size))
}

#[cfg(test)]
mod tests {
    use super::decode_static;
    use std::io::{Cursor, Write};

    /// Regression guard for the "JPEG bytes, .png name" bug: MulVie must decode
    /// by content, not extension. Extension-based decoding would fail here.
    #[test]
    fn decodes_jpeg_content_despite_png_extension() {
        let img = image::RgbImage::from_pixel(6, 4, image::Rgb([10, 120, 60]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "test data isn't JPEG");

        let path = std::env::temp_dir().join(format!("mulvie_jpg_in_{}.png", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(&jpeg).unwrap();
        let res = decode_static(&path);
        let _ = std::fs::remove_file(&path);

        let (_frames, size) = res.expect("content-based decode of jpeg-in-.png failed");
        assert_eq!(size, [6, 4]);
    }
}
