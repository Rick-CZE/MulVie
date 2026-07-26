//! Embedded video playback via libmpv, rendered into an egui-owned GL texture.
//!
//! The FBO texture is SRGB8_ALPHA8 so egui_glow's sRGB sampling round-trips to
//! correct exposure (a plain RGBA8 texture double-encodes and looks washed
//! out). Zoom/pan are done MulVie-side (the FBO is rendered at zoom resolution
//! so it stays sharp). RenderContext borrows Mpv, so both are kept together via
//! a lifetime transmute with `render` dropping before `mpv`.

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Pos2, Rect, TextureId, Vec2};
use eframe::glow::{self, HasContext as _};
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::Mpv;

const INIT_W: i32 = 1280;
const INIT_H: i32 = 720;
const MAX_W: i32 = 3840;
const MAX_H: i32 = 2160;

pub const ADJUST_NAMES: [&str; 5] = ["brightness", "contrast", "saturation", "gamma", "hue"];

pub fn probe() -> bool {
    Mpv::new().is_ok()
}

/// What a pane backed by a `VideoPlayer` should draw this frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visual {
    /// A video track is present (a real stream or embedded cover art): draw the
    /// mpv frame.
    Frame,
    /// The file is loaded and has audio but no video: draw the music note.
    Note,
    /// Tracks not parsed yet (still loading): draw nothing but the canvas, so a
    /// real video never flashes a music note during its brief load window.
    Loading,
}

/// One selectable audio or subtitle track, as the right-click menu shows it.
#[derive(Clone)]
pub struct Track {
    /// mpv's per-type track id (what `aid` / `sid` select by).
    pub id: i64,
    /// Display name: the track's own title, else its language, else "Track N".
    pub label: String,
    /// Whether this track is the one mpv is currently using.
    pub selected: bool,
}

/// The menu label for a track: what the video calls it (title, then language),
/// falling back to the track number when it carries no metadata.
fn track_label(title: Option<String>, lang: Option<String>, id: i64) -> String {
    title
        .filter(|s| !s.trim().is_empty())
        .or_else(|| lang.filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| format!("Track {id}"))
}

pub struct VideoPlayer {
    // Field order == drop order: `render` MUST drop before `mpv`.
    render: RenderContext<'static>,
    mpv: Mpv,
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    pub tex_id: TextureId,
    w: i32,
    h: i32,
    last_target: (i32, i32),

    pub file: Option<PathBuf>,
    pub muted: bool,
    paused: bool,
    pub user_paused: bool,

    // Draw-side zoom/pan (points). The FBO is rendered at zoom resolution.
    pub zoom: f32,
    pub pan: Vec2,

    pub volume: f64,
    pub adjust: [i64; 5],
    /// Start time of the active A-B loop, if any.
    pub loop_start: Option<f64>,
    /// Clockwise view rotation in degrees (0/90/180/270), reset per clip.
    rotate: u16,
    /// Playback speed multiplier (1.0 = 100%), reset per clip.
    pub speed: f64,
}

impl VideoPlayer {
    pub fn new(
        gl: &glow::Context,
        frame: &mut eframe::Frame,
        egui_ctx: &egui::Context,
    ) -> Option<Self> {
        let (tex, fbo) = unsafe {
            let tex = gl.create_texture().ok()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            alloc_srgb(gl, INIT_W, INIT_H);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            let fbo = gl.create_framebuffer().ok()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            (tex, fbo)
        };

        let mpv = Mpv::with_initializer(|init| {
            let _ = init.set_option("vo", "libmpv");
            // --- Privacy hardening: write NOTHING to the host machine ---------
            // Portable/USB app; writing nothing outside its own folder is the #1
            // requirement. libmpv2's set_option returns Err on an unknown/renamed
            // option, and terminal=false hides stderr, so the disk-write guards
            // below use `?` (not `let _ =`): if a future libmpv-2.dll renames one,
            // video init fails loudly (pane shows no video) rather than silently
            // leaking caches to the host. Option names validated against the
            // shipped mpv 0.41.0. NOTE: config=false alone does NOT stop the
            // shader/ICC caches — each needs its own switch.
            init.set_option("config", false)?; // don't read the host's mpv.conf
            init.set_option("gpu-shader-cache", false)?; // default YES: else caches GLSL shaders to %APPDATA%\mpv on exit
            init.set_option("icc-cache", false)?; // default YES: else caches ICC/3DLUT profiles to disk
            // A crafted media file must not make the demuxer open OTHER files or
            // URLs it references (QuickTime reference movies, HLS-style nested
            // streams, …). MulVie only ever plays the one local file it was given.
            init.set_option("access-references", false)?;
            // These already default to safe; pin them best-effort (a wrong name is
            // harmless here since the default doesn't write).
            let _ = init.set_option("cache-on-disk", false); // keep the demuxer cache in RAM only; `cache` itself stays on for smooth seek/A-B loop
            let _ = init.set_option("save-position-on-quit", false); // never write watch_later resume files
            let _ = init.set_option("resume-playback", false); // and don't even read host watch_later state
            let _ = init.set_option("load-scripts", false); // no auto-loaded script can write to disk
            // -----------------------------------------------------------------
            // Subtitles: still auto-LOAD external subs sitting next to the video
            // (fuzzy = any sub whose name contains the clip's), but display NONE
            // by default. `sid=no` is the per-file default, so every clip starts
            // with subtitles off until the user picks one from the menu.
            let _ = init.set_option("sub-auto", "fuzzy");
            let _ = init.set_option("sid", "no");
            // Cover art: show a file's OWN embedded artwork (audio-display keeps
            // its default `embedded-first`), but do NOT auto-adopt a stray
            // image sitting in the folder (cover.png / folder.jpg / …). Without
            // this, a pure audio file next to such an image would show that
            // image instead of the music note — surprising, and against the
            // rule that the note appears for files with no embedded art.
            let _ = init.set_option("cover-art-auto", "no");
            let _ = init.set_option("terminal", false);
            let _ = init.set_option("osc", false);
            let _ = init.set_option("input-default-bindings", false);
            let _ = init.set_option("keepaspect", false);
            let _ = init.set_option("loop-file", "inf");
            let _ = init.set_option("hwdec", "no");
            let _ = init.set_option("mute", true);
            let _ = init.set_option("video-timing-offset", 0i64);
            let _ = init.set_option("profile", "high-quality");
            let _ = init.set_option("scale", "ewa_lanczossharp");
            let _ = init.set_option("cscale", "ewa_lanczossharp");
            let _ = init.set_option("dscale", "mitchell");
            let _ = init.set_option("correct-downscaling", true);
            let _ = init.set_option("linear-downscaling", true);
            let _ = init.set_option("sigmoid-upscaling", true);
            let _ = init.set_option("deband", true);
            let _ = init.set_option("dither-depth", "auto");
            Ok(())
        })
        .ok()?;

        let render = mpv
            .create_render_context::<()>(vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: (),
                }),
            ])
            .ok()?;
        let mut render: RenderContext<'static> =
            unsafe { std::mem::transmute::<RenderContext<'_>, RenderContext<'static>>(render) };

        let repaint_ctx = egui_ctx.clone();
        render.set_update_callback(move || repaint_ctx.request_repaint());

        let tex_id = frame.register_native_glow_texture(tex);

        Some(Self {
            render,
            mpv,
            fbo,
            tex,
            tex_id,
            w: INIT_W,
            h: INIT_H,
            last_target: (INIT_W, INIT_H),
            file: None,
            muted: true,
            paused: false,
            user_paused: false,
            zoom: 1.0,
            pan: Vec2::ZERO,
            volume: 100.0,
            adjust: [0; 5],
            loop_start: None,
            rotate: 0,
            speed: 1.0,
        })
    }

    fn load_target(&mut self, target: &str) {
        let _ = self.mpv.command("loadfile", &[target, "replace"]);
        self.user_paused = false;
        self.paused = false;
        let _ = self.mpv.set_property("pause", false);
        self.reset_view(); // new clip -> reset zoom/pan
        self.clear_ab_loop();
        // New clip starts upright and at normal speed.
        self.rotate = 0;
        let _ = self.mpv.set_property("video-rotate", 0i64);
        self.set_speed(1.0);
    }

    /// Set the playback speed multiplier (1.0 = 100%), clamped to 1%..500%.
    /// The A-B loop is defined in media time, so it is unaffected by speed.
    pub fn set_speed(&mut self, mult: f64) {
        self.speed = crate::widgets::clamp_speed(mult);
        let _ = self.mpv.set_property("speed", self.speed);
    }

    /// Rotate the view 90° clockwise (mpv re-renders, so it stays sharp).
    pub fn rotate_cw(&mut self) {
        self.set_rotate((self.rotate + 90) % 360);
    }

    /// Rotate the view 90° counter-clockwise.
    pub fn rotate_ccw(&mut self) {
        self.set_rotate((self.rotate + 270) % 360);
    }

    fn set_rotate(&mut self, deg: u16) {
        self.rotate = deg;
        let _ = self.mpv.set_property("video-rotate", self.rotate as i64);
        self.reset_view(); // zoom/pan don't survive an axis swap meaningfully
    }

    pub fn load_path(&mut self, path: &Path) {
        // Windows: mpv prefers forward slashes. On Linux a backslash is a
        // LEGAL filename character — rewriting it would break such files.
        let target = if cfg!(windows) {
            path.to_string_lossy().replace('\\', "/")
        } else {
            path.to_string_lossy().into_owned()
        };
        self.load_target(&target);
        self.file = Some(path.to_path_buf());
    }

    // --- Audio / subtitle tracks -----------------------------------------

    /// The tracks of one `kind` ("audio" or "sub") from mpv's live track-list.
    /// Read fresh when the menu opens — by then the clip is loaded and any
    /// external `.srt` next to it has been auto-added.
    pub fn tracks(&self, kind: &str) -> Vec<Track> {
        let count = self.mpv.get_property::<i64>("track-list/count").unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..count {
            let t = self
                .mpv
                .get_property::<String>(&format!("track-list/{i}/type"))
                .unwrap_or_default();
            if t != kind {
                continue;
            }
            let Ok(id) = self.mpv.get_property::<i64>(&format!("track-list/{i}/id")) else {
                continue;
            };
            let title = self
                .mpv
                .get_property::<String>(&format!("track-list/{i}/title"))
                .ok();
            let lang = self
                .mpv
                .get_property::<String>(&format!("track-list/{i}/lang"))
                .ok();
            let selected = self
                .mpv
                .get_property::<bool>(&format!("track-list/{i}/selected"))
                .unwrap_or(false);
            out.push(Track {
                id,
                label: track_label(title, lang, id),
                selected,
            });
        }
        out
    }

    /// What the pane should draw, decided from mpv's live track-list. Keyed on
    /// the file's actual streams (not its extension), so an audio-only file that
    /// happens to carry a video-container extension still gets the music note
    /// rather than a blank pane. The track-list is populated once the headers
    /// are parsed (right after `loadfile`); until then it is empty and the pane
    /// stays on the plain canvas, so a real video never flashes a note mid-load.
    pub fn visual_state(&self) -> Visual {
        let count = self.mpv.get_property::<i64>("track-list/count").unwrap_or(0);
        if count == 0 {
            return Visual::Loading;
        }
        let mut has_audio = false;
        for i in 0..count {
            match self
                .mpv
                .get_property::<String>(&format!("track-list/{i}/type"))
                .as_deref()
            {
                // A real video stream OR embedded cover art (mpv lists artwork
                // as a still video track) — draw the frame either way.
                Ok("video") => return Visual::Frame,
                Ok("audio") => has_audio = true,
                _ => {}
            }
        }
        if has_audio {
            Visual::Note
        } else {
            Visual::Loading
        }
    }

    /// True while the clip is actively advancing (not paused). Used to decide
    /// whether to keep the machine awake.
    pub fn is_playing(&self) -> bool {
        !self.paused
    }

    /// True if the file has a real, moving video track — as opposed to only
    /// audio and/or a still cover-art image (mpv lists cover art as a video
    /// track flagged `albumart`). Lets the keep-awake logic keep the display on
    /// for actual video while letting the screen sleep for music.
    pub fn has_moving_video(&self) -> bool {
        let count = self.mpv.get_property::<i64>("track-list/count").unwrap_or(0);
        (0..count).any(|i| {
            let is_video = matches!(
                self.mpv
                    .get_property::<String>(&format!("track-list/{i}/type"))
                    .as_deref(),
                Ok("video")
            );
            is_video
                && !self
                    .mpv
                    .get_property::<bool>(&format!("track-list/{i}/albumart"))
                    .unwrap_or(false)
        })
    }

    /// Switch the active audio track to `id`.
    pub fn set_audio(&mut self, id: i64) {
        let _ = self.mpv.set_property("aid", id);
    }

    /// Show subtitle track `id`, or turn subtitles off with `None`.
    pub fn set_sub(&mut self, id: Option<i64>) {
        match id {
            Some(id) => {
                let _ = self.mpv.set_property("sid", id);
            }
            None => {
                let _ = self.mpv.set_property("sid", "no");
            }
        }
    }

    // --- Playback / audio ------------------------------------------------

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        let _ = self.mpv.set_property("mute", muted);
    }

    pub fn set_volume(&mut self, v: f64) {
        self.volume = v.clamp(0.0, 130.0);
        let _ = self.mpv.set_property("volume", self.volume);
    }

    pub fn toggle_pause(&mut self) {
        self.user_paused = !self.user_paused;
        self.ensure_active(true);
    }

    pub fn ensure_active(&mut self, active: bool) {
        let want = if active { self.user_paused } else { true };
        if self.paused != want {
            self.paused = want;
            let _ = self.mpv.set_property("pause", want);
        }
    }

    /// STOP: rewind to the loop start (or 0) and pause. Pressing it again while
    /// already at the loop start discards the loop and rewinds fully.
    pub fn stop(&mut self) {
        let t = self.mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
        match self.loop_start {
            Some(a) if (t - a).abs() > 0.15 => {
                let _ = self.mpv.set_property("time-pos", a);
            }
            Some(_) => {
                self.clear_ab_loop();
                let _ = self.mpv.set_property("time-pos", 0.0);
            }
            None => {
                let _ = self.mpv.set_property("time-pos", 0.0);
            }
        }
        self.user_paused = true;
        self.paused = true;
        let _ = self.mpv.set_property("pause", true);
    }

    pub fn seek_fraction(&mut self, f: f64) {
        if let Ok(dur) = self.mpv.get_property::<f64>("duration") {
            if dur > 0.0 {
                let _ = self.mpv.set_property("time-pos", f.clamp(0.0, 1.0) * dur);
            }
        }
    }

    pub fn progress(&self) -> Option<(f64, f64)> {
        let pos = self.mpv.get_property::<f64>("time-pos").ok()?;
        let dur = self.mpv.get_property::<f64>("duration").ok()?;
        Some((pos, dur))
    }

    pub fn aspect(&self) -> Option<f64> {
        self.mpv
            .get_property::<f64>("video-params/aspect")
            .ok()
            .filter(|a| *a > 0.0)
            // A 90°/270° view rotation swaps the displayed axes.
            .map(|a| if self.rotate % 180 == 90 { 1.0 / a } else { a })
    }

    // --- A-B loop --------------------------------------------------------

    pub fn set_ab_loop(&mut self, secs: f64) {
        if let Ok(t) = self.mpv.get_property::<f64>("time-pos") {
            let a = (t - secs).max(0.0);
            let _ = self.mpv.set_property("ab-loop-b", t);
            let _ = self.mpv.set_property("ab-loop-a", a);
            let _ = self.mpv.set_property("ab-loop-count", "inf");
            self.loop_start = Some(a);
        }
    }

    pub fn clear_ab_loop(&mut self) {
        let _ = self.mpv.set_property("ab-loop-a", "no");
        let _ = self.mpv.set_property("ab-loop-b", "no");
        self.loop_start = None;
    }

    // --- Video adjustments -----------------------------------------------

    pub fn set_adjust(&mut self, idx: usize, val: i64) {
        if idx < 5 {
            self.adjust[idx] = val.clamp(-100, 100);
            let _ = self.mpv.set_property(ADJUST_NAMES[idx], self.adjust[idx]);
        }
    }

    pub fn reset_adjust(&mut self) {
        for i in 0..5 {
            self.set_adjust(i, 0);
        }
    }

    // --- Zoom / pan (draw-side) ------------------------------------------

    pub fn is_zoomed(&self) -> bool {
        self.zoom > 1.0001
    }

    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Cursor-anchored zoom. `disp` is the video's on-screen rect at zoom 1.
    pub fn zoom_at(&mut self, disp: Rect, cursor: Pos2, factor: f32) {
        let old = self.zoom;
        let new = (old * factor).clamp(1.0, 8.0);
        if (new - old).abs() < f32::EPSILON {
            return;
        }
        let o = disp.center().to_vec2();
        let c = cursor.to_vec2();
        self.pan = (c - o) - ((c - o) - self.pan) * (new / old);
        self.zoom = new;
        if self.zoom <= 1.0001 {
            self.pan = Vec2::ZERO;
        }
        self.clamp_pan(disp);
    }

    pub fn pan_by(&mut self, disp: Rect, delta: Vec2) {
        self.pan += delta;
        self.clamp_pan(disp);
    }

    fn clamp_pan(&mut self, disp: Rect) {
        let scaled = disp.size() * self.zoom;
        let mx = ((scaled.x - disp.width()) * 0.5).max(0.0);
        let my = ((scaled.y - disp.height()) * 0.5).max(0.0);
        self.pan.x = self.pan.x.clamp(-mx, mx);
        self.pan.y = self.pan.y.clamp(-my, my);
    }

    // --- Rendering -------------------------------------------------------

    pub fn ensure_size(&mut self, gl: &glow::Context, tw: i32, th: i32) {
        let tw = tw.clamp(16, MAX_W);
        let th = th.clamp(16, MAX_H);
        if (tw, th) == (self.w, self.h) {
            return;
        }
        if (tw, th) != self.last_target {
            self.last_target = (tw, th);
            return;
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
            alloc_srgb(gl, tw, th);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        self.w = tw;
        self.h = th;
    }

    pub fn render_frame(&self, gl: &glow::Context) {
        let fbo_id = self.fbo.0.get() as i32;
        let _ = self.render.render::<()>(fbo_id, self.w, self.h, false);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // libmpv2's Mpv::drop uses mpv_destroy (async), which can leave audio
        // playing. `command("stop")` is synchronous, so it silences the clip
        // before the handle is torn down.
        let _ = self.mpv.command("stop", &[]);
    }
}

unsafe fn alloc_srgb(gl: &glow::Context, w: i32, h: i32) {
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::SRGB8_ALPHA8 as i32,
        w,
        h,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::track_label;

    #[test]
    fn track_label_prefers_title_then_lang_then_number() {
        assert_eq!(
            track_label(Some("Commentary".into()), Some("eng".into()), 2),
            "Commentary"
        );
        assert_eq!(track_label(None, Some("cze".into()), 3), "cze");
        assert_eq!(track_label(None, None, 4), "Track 4");
        // Blank/whitespace metadata is treated as absent.
        assert_eq!(track_label(Some("  ".into()), None, 1), "Track 1");
        assert_eq!(track_label(Some("".into()), Some("eng".into()), 1), "eng");
    }
}

/// Resolve a GL symbol for mpv's render API. mpv calls this on its render
/// thread with the app's GL context current, so the platform's standard
/// loader chain applies.
#[cfg(windows)]
fn get_proc_address(_ctx: &(), name: &str) -> *mut c_void {
    use windows_sys::Win32::Graphics::OpenGL::wglGetProcAddress;
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleA, GetProcAddress, LoadLibraryA,
    };

    let Ok(cname) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    let namep = cname.as_ptr() as *const u8;
    unsafe {
        if let Some(f) = wglGetProcAddress(namep) {
            let a = f as usize;
            if a > 3 && a != usize::MAX {
                return f as *mut c_void;
            }
        }
        let mut h = GetModuleHandleA(b"opengl32.dll\0".as_ptr());
        if (h as isize) == 0 {
            h = LoadLibraryA(b"opengl32.dll\0".as_ptr());
        }
        match GetProcAddress(h, namep) {
            Some(f) => f as *mut c_void,
            None => std::ptr::null_mut(),
        }
    }
}

/// Unix twin: GLX first (glXGetProcAddressARB — eframe 0.29 prefers a GLX
/// context on X11, so match it; GLVND dispatches per-context either way, but
/// a non-GLVND vendor libEGL could hand out EGL-only pointers), then EGL,
/// then a plain dlsym. A library only becomes THE loader if its getter symbol
/// actually resolves — otherwise the next candidate is tried, so a quirky
/// libEGL can't strand every lookup in the wrong library. Libraries are
/// dlopened once and deliberately leaked — they must outlive every later
/// lookup mpv makes.
#[cfg(unix)]
fn get_proc_address(_ctx: &(), name: &str) -> *mut c_void {
    use std::sync::OnceLock;

    type GetProc = unsafe extern "C" fn(*const std::os::raw::c_char) -> *mut c_void;
    struct Loader {
        get_proc: Option<GetProc>,
        lib: Option<&'static libloading::Library>,
    }
    static LOADER: OnceLock<Loader> = OnceLock::new();

    let loader = LOADER.get_or_init(|| {
        let mut first_lib: Option<&'static libloading::Library> = None;
        for (libname, symbol) in [
            ("libGL.so.1", b"glXGetProcAddressARB\0".as_slice()),
            ("libEGL.so.1", b"eglGetProcAddress\0".as_slice()),
        ] {
            if let Ok(lib) = unsafe { libloading::Library::new(libname) } {
                let lib: &'static libloading::Library = Box::leak(Box::new(lib));
                if let Some(get_proc) = unsafe { lib.get::<GetProc>(symbol).ok().map(|s| *s) } {
                    return Loader {
                        get_proc: Some(get_proc),
                        lib: Some(lib),
                    };
                }
                first_lib.get_or_insert(lib); // dlsym-only fallback candidate
            }
        }
        Loader {
            get_proc: None,
            lib: first_lib,
        }
    });

    let Ok(cname) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    unsafe {
        if let Some(get_proc) = loader.get_proc {
            let p = get_proc(cname.as_ptr());
            if !p.is_null() {
                return p;
            }
        }
        // Final fallback: an ordinary dynamic-symbol lookup in the GL library.
        if let Some(lib) = loader.lib {
            if let Ok(sym) = lib.get::<unsafe extern "C" fn()>(cname.as_bytes_with_nul()) {
                return *sym as usize as *mut c_void;
            }
        }
        std::ptr::null_mut()
    }
}
