//! Platform eye-candy. Currently: Windows "acrylic" (frosted-glass) blur-behind.
//!
//! `SetWindowCompositionAttribute` is an undocumented but stable user32 export,
//! so we resolve it at runtime and no-op if unavailable.

/// Enable acrylic blur-behind on the given HWND with an ABGR tint
/// (0xAABBGGRR). Returns true on success.
#[cfg(windows)]
pub fn enable_acrylic(hwnd: isize, tint_abgr: u32) -> bool {
    use std::ffi::c_void;

    #[repr(C)]
    struct AccentPolicy {
        accent_state: u32,
        accent_flags: u32,
        gradient_color: u32,
        animation_id: u32,
    }
    #[repr(C)]
    struct WinCompAttrData {
        attribute: u32,
        data: *mut c_void,
        size: usize,
    }
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
    const WCA_ACCENT_POLICY: u32 = 19;

    unsafe {
        let lib = match libloading::Library::new("user32.dll") {
            Ok(l) => l,
            Err(_) => return false,
        };
        let func: libloading::Symbol<unsafe extern "system" fn(isize, *mut WinCompAttrData) -> i32> =
            match lib.get(b"SetWindowCompositionAttribute\0") {
                Ok(f) => f,
                Err(_) => return false,
            };
        let mut accent = AccentPolicy {
            accent_state: ACCENT_ENABLE_ACRYLICBLURBEHIND,
            accent_flags: 0,
            gradient_color: tint_abgr,
            animation_id: 0,
        };
        let mut data = WinCompAttrData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut accent as *mut _ as *mut c_void,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        func(hwnd, &mut data) != 0
    }
}

#[cfg(not(windows))]
pub fn enable_acrylic(_hwnd: isize, _tint_abgr: u32) -> bool {
    false
}

/// Make the OS title bar dark to match MulVie's chrome (instead of the default
/// light Windows caption).
#[cfg(windows)]
pub fn enable_dark_titlebar(hwnd: isize) {
    use std::ffi::c_void;
    unsafe {
        let Ok(lib) = libloading::Library::new("dwmapi.dll") else {
            return;
        };
        let func: Result<
            libloading::Symbol<unsafe extern "system" fn(isize, u32, *const c_void, u32) -> i32>,
            _,
        > = lib.get(b"DwmSetWindowAttribute\0");
        let Ok(func) = func else { return };
        let on: i32 = 1;
        // 20 = DWMWA_USE_IMMERSIVE_DARK_MODE (Win10 2004+); 19 on older builds.
        for attr in [20u32, 19u32] {
            func(hwnd, attr, &on as *const _ as *const c_void, 4);
        }
    }
}

#[cfg(not(windows))]
pub fn enable_dark_titlebar(_hwnd: isize) {}

/// The mouse position in the given window's client coordinates (physical
/// pixels). Works even during an OS drag-and-drop, when the app itself gets no
/// mouse events — which is exactly when we need it (winit drops the drop
/// coordinates on Windows).
#[cfg(windows)]
pub fn cursor_pos_in_client(hwnd: isize) -> Option<(f32, f32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    unsafe {
        let lib = libloading::Library::new("user32.dll").ok()?;
        let get_cursor_pos: libloading::Symbol<unsafe extern "system" fn(*mut Point) -> i32> =
            lib.get(b"GetCursorPos\0").ok()?;
        let screen_to_client: libloading::Symbol<
            unsafe extern "system" fn(isize, *mut Point) -> i32,
        > = lib.get(b"ScreenToClient\0").ok()?;
        let mut p = Point { x: 0, y: 0 };
        if get_cursor_pos(&mut p) == 0 {
            return None;
        }
        if screen_to_client(hwnd, &mut p) == 0 {
            return None;
        }
        Some((p.x as f32, p.y as f32))
    }
}

/// Linux/X11: `XQueryPointer` against our own window — a QUERY, so it works
/// mid-XDND-drag (winit deliberately discards drop coordinates on X11, the
/// same stale-pointer bug this fixed on Windows). Frameless window → client
/// coordinates == window coordinates. Physical pixels, like the Windows twin.
#[cfg(target_os = "linux")]
pub fn cursor_pos_in_client(window: isize) -> Option<(f32, f32)> {
    unsafe {
        let x11 = x11()?;
        let dpy = (x11.open)(std::ptr::null());
        if dpy.is_null() {
            return None;
        }
        let (mut root, mut child) = (0u64, 0u64);
        let (mut rx, mut ry, mut wx, mut wy) = (0i32, 0i32, 0i32, 0i32);
        let mut mask = 0u32;
        let ok = (x11.query_pointer)(
            dpy,
            window as u64,
            &mut root,
            &mut child,
            &mut rx,
            &mut ry,
            &mut wx,
            &mut wy,
            &mut mask,
        );
        (x11.close)(dpy);
        (ok != 0).then(|| (wx as f32, wy as f32))
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn cursor_pos_in_client(_hwnd: isize) -> Option<(f32, f32)> {
    None
}

/// Lazily dlopen'd libX11 entry points (mirrors the user32 libloading style;
/// the library is leaked so the symbols stay valid for the app's lifetime).
#[cfg(target_os = "linux")]
struct X11 {
    open: unsafe extern "C" fn(*const std::os::raw::c_char) -> *mut std::ffi::c_void,
    close: unsafe extern "C" fn(*mut std::ffi::c_void) -> i32,
    #[allow(clippy::type_complexity)]
    query_pointer: unsafe extern "C" fn(
        *mut std::ffi::c_void,
        u64,
        *mut u64,
        *mut u64,
        *mut i32,
        *mut i32,
        *mut i32,
        *mut i32,
        *mut u32,
    ) -> i32,
    default_root: unsafe extern "C" fn(*mut std::ffi::c_void) -> u64,
    intern_atom:
        unsafe extern "C" fn(*mut std::ffi::c_void, *const std::os::raw::c_char, i32) -> u64,
    define_cursor: unsafe extern "C" fn(*mut std::ffi::c_void, u64, u64) -> i32,
    #[allow(clippy::type_complexity)]
    change_property: unsafe extern "C" fn(
        *mut std::ffi::c_void,
        u64,
        u64,
        u64,
        i32,
        i32,
        *const u8,
        i32,
    ) -> i32,
}

#[cfg(target_os = "linux")]
fn x11() -> Option<&'static X11> {
    use std::sync::OnceLock;
    static LIB: OnceLock<Option<X11>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libX11.so.6").ok()?;
        let lib: &'static libloading::Library = Box::leak(Box::new(lib));
        Some(X11 {
            open: *lib.get(b"XOpenDisplay\0").ok()?,
            close: *lib.get(b"XCloseDisplay\0").ok()?,
            query_pointer: *lib.get(b"XQueryPointer\0").ok()?,
            default_root: *lib.get(b"XDefaultRootWindow\0").ok()?,
            intern_atom: *lib.get(b"XInternAtom\0").ok()?,
            change_property: *lib.get(b"XChangeProperty\0").ok()?,
            define_cursor: *lib.get(b"XDefineCursor\0").ok()?,
        })
    })
    .as_ref()
}

/// Linux/X11: hang the app icon on the window ourselves via `_NET_WM_ICON`.
/// winit sets the property too, but with a single 256px image and errors
/// silently ignored — and Cinnamon's icon path (window-backed apps read the
/// window's icon property, there being no installed .desktop file to match)
/// has shown a generic gear in practice. Re-setting it after startup with a
/// PROPER multi-size pack (Muffin's `find_best_size` picks per consumer:
/// ~24px panel, ~64px Alt-Tab) is portable — no file touches the host — and
/// also survives any winit-side failure. No-op off X11 (Wayland has no
/// client-set-icon protocol at all; there the icon would need an installed
/// .desktop file, which a portable app must not write).
#[cfg(target_os = "linux")]
pub fn set_x11_window_icon(window: isize) {
    use std::os::raw::{c_char, c_ulong};
    let Ok(img) = image::load_from_memory(include_bytes!("../assets/icon.png")) else {
        return;
    };
    let img = img.to_rgba8();
    // Xlib format-32 properties travel as C longs (64-bit here), one CARDINAL
    // per long: [w, h, ARGB pixels…] repeated per size, all in one property.
    // The source's own size goes in unscaled at the end — REPLACE drops the
    // 256px image winit set, and HiDPI consumers (2× Alt-Tab, big dock icons)
    // should downscale a crisp 256, not upscale our 128.
    let mut data: Vec<c_ulong> = Vec::new();
    for size in [16u32, 24, 32, 48, 64, 128, img.width().max(img.height())] {
        let scaled = if size == img.width() && size == img.height() {
            img.clone()
        } else {
            image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3)
        };
        data.push(u64::from(scaled.width()));
        data.push(u64::from(scaled.height()));
        for px in scaled.pixels() {
            let [r, g, b, a] = px.0;
            data.push(u64::from(
                (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
            ));
        }
    }
    unsafe {
        let Some(x11) = x11() else { return };
        let dpy = (x11.open)(std::ptr::null());
        if dpy.is_null() {
            return;
        }
        let atom = (x11.intern_atom)(dpy, b"_NET_WM_ICON\0".as_ptr() as *const c_char, 0);
        const XA_CARDINAL: u64 = 6;
        const PROP_MODE_REPLACE: i32 = 0;
        if atom != 0 {
            (x11.change_property)(
                dpy,
                window as u64,
                atom,
                XA_CARDINAL,
                32,
                PROP_MODE_REPLACE,
                data.as_ptr() as *const u8,
                data.len() as i32,
            );
        }
        (x11.close)(dpy); // XCloseDisplay flushes the request buffer
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_x11_window_icon(_window: isize) {}

/// Linux/X11: drop the window's own cursor attribute (revert to the parent's
/// default arrow). Recovery for WM-driven move/resize: the WM's grab swallows
/// the events egui-winit needs to notice the pointer, so it can skip the real
/// `XDefineCursor` call while still CACHING the icon as applied — leaving the
/// resize arrows stuck on the window until some unrelated cursor change.
/// Clearing the attribute directly resyncs reality with that cache (which
/// believes "default" is current), and later per-widget changes flow normally.
#[cfg(target_os = "linux")]
pub fn define_cursor_default(window: isize) {
    unsafe {
        let Some(x11) = x11() else { return };
        let dpy = (x11.open)(std::ptr::null());
        if dpy.is_null() {
            return;
        }
        (x11.define_cursor)(dpy, window as u64, 0); // 0 = None -> inherit
        (x11.close)(dpy); // flushes
    }
}

#[cfg(not(target_os = "linux"))]
pub fn define_cursor_default(_window: isize) {}

/// Whether the mouse cursor is currently over the given native window — the
/// VISIBLE one under the pointer, not merely inside its rectangle. Used by
/// the List-Management drag-and-drop to make sure a release actually happened
/// over MulVie's MAIN window: a browser window may overlap it, and a drop
/// onto that window must not fall through to the pane behind.
#[cfg(windows)]
pub fn cursor_over_window(target: isize) -> bool {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    const GA_ROOT: u32 = 2;
    unsafe {
        let Ok(lib) = libloading::Library::new("user32.dll") else {
            return false;
        };
        let Ok(get_cursor_pos) =
            lib.get::<unsafe extern "system" fn(*mut Point) -> i32>(b"GetCursorPos\0")
        else {
            return false;
        };
        let Ok(window_from_point) =
            lib.get::<unsafe extern "system" fn(Point) -> isize>(b"WindowFromPoint\0")
        else {
            return false;
        };
        let Ok(get_ancestor) =
            lib.get::<unsafe extern "system" fn(isize, u32) -> isize>(b"GetAncestor\0")
        else {
            return false;
        };
        let mut p = Point { x: 0, y: 0 };
        if get_cursor_pos(&mut p) == 0 {
            return false;
        }
        let hwnd = window_from_point(p);
        if hwnd == 0 {
            return false;
        }
        // Map a child window to its top-level ancestor so the equality check
        // against the main window handle works wherever the point landed.
        let root = get_ancestor(hwnd, GA_ROOT);
        (if root != 0 { root } else { hwnd }) == target
    }
}

/// Linux/X11: descend from the root along the pointer-containing (topmost)
/// child chain — X returns the topmost viewable child at each level, so this
/// follows the VISIBLE window stack. The target is "under the cursor" when it
/// appears anywhere in that chain (under a reparenting WM like Muffin the
/// chain passes through the frame window before reaching our client window).
#[cfg(target_os = "linux")]
pub fn cursor_over_window(target: isize) -> bool {
    let target = target as u64;
    unsafe {
        let Some(x11) = x11() else { return false };
        let dpy = (x11.open)(std::ptr::null());
        if dpy.is_null() {
            return false;
        }
        let mut over = false;
        let mut w = (x11.default_root)(dpy);
        for _ in 0..64 {
            // depth guard; real chains are a handful deep
            let (mut root, mut child) = (0u64, 0u64);
            let (mut rx, mut ry, mut wx, mut wy) = (0i32, 0i32, 0i32, 0i32);
            let mut mask = 0u32;
            if (x11.query_pointer)(
                dpy, w, &mut root, &mut child, &mut rx, &mut ry, &mut wx, &mut wy, &mut mask,
            ) == 0
            {
                break;
            }
            if child == 0 {
                break;
            }
            if child == target {
                over = true;
                break;
            }
            w = child;
        }
        (x11.close)(dpy);
        over
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn cursor_over_window(_target: isize) -> bool {
    false
}

/// Find a top-level window by its exact title (used to reach the rename
/// window's HWND, which eframe doesn't expose for child viewports).
#[cfg(windows)]
pub fn find_window_by_title(title: &str) -> Option<isize> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let lib = libloading::Library::new("user32.dll").ok()?;
        let find: libloading::Symbol<unsafe extern "system" fn(*const u16, *const u16) -> isize> =
            lib.get(b"FindWindowW\0").ok()?;
        let hwnd = find(std::ptr::null(), wide.as_ptr());
        (hwnd != 0).then_some(hwnd)
    }
}

#[cfg(not(windows))]
pub fn find_window_by_title(_title: &str) -> Option<isize> {
    None
}

/// Bring a window to the foreground (restoring it first if minimized).
#[cfg(windows)]
pub fn focus_window(hwnd: isize) {
    const SW_RESTORE: i32 = 9;
    unsafe {
        let Ok(lib) = libloading::Library::new("user32.dll") else {
            return;
        };
        if let Ok(is_iconic) =
            lib.get::<unsafe extern "system" fn(isize) -> i32>(b"IsIconic\0")
        {
            if is_iconic(hwnd) != 0 {
                if let Ok(show) =
                    lib.get::<unsafe extern "system" fn(isize, i32) -> i32>(b"ShowWindow\0")
                {
                    show(hwnd, SW_RESTORE);
                }
            }
        }
        if let Ok(set_fg) =
            lib.get::<unsafe extern "system" fn(isize) -> i32>(b"SetForegroundWindow\0")
        {
            set_fg(hwnd);
        }
    }
}

#[cfg(not(windows))]
pub fn focus_window(_hwnd: isize) {}

/// True if another MulVie PROCESS is already running (via a named kernel mutex).
/// This is process-based on purpose: a window-title search would collide with
/// unrelated windows that happen to be titled "MulVie" (e.g. a File Explorer
/// window open on a folder named MulVie). The handle is leaked so the claim lasts
/// the whole process; the OS releases it on exit.
#[cfg(windows)]
pub fn another_instance_running() -> bool {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let name: Vec<u16> = "Local\\MulVie_SingleInstance\0".encode_utf16().collect();
    unsafe {
        let Ok(lib) = libloading::Library::new("kernel32.dll") else {
            return false; // can't tell — behave as the primary instance
        };
        let Ok(create) = lib.get::<unsafe extern "system" fn(
            *const std::ffi::c_void,
            i32,
            *const u16,
        ) -> isize>(b"CreateMutexW\0") else {
            return false;
        };
        let Ok(last_err) = lib.get::<unsafe extern "system" fn() -> u32>(b"GetLastError\0") else {
            return false;
        };
        let handle = create(std::ptr::null(), 0, name.as_ptr());
        // GetLastError is read immediately after CreateMutexW with nothing in
        // between, so it reflects the mutex creation.
        let already = last_err() == ERROR_ALREADY_EXISTS;
        if handle != 0 {
            std::mem::forget(lib); // keep the mutex handle alive for our lifetime
        }
        already
    }
}

/// Linux: an abstract Unix socket plays the part of the named kernel mutex —
/// same idea (a kernel-owned name, auto-released when the process dies, zero
/// disk residue), same single-claim semantics. Bind wins = primary instance.
/// The name embeds user + display because the abstract namespace is
/// MACHINE-global — a bare name would make two logged-in users collide (the
/// second user's file-open would hand off into the first user's session and
/// show nothing). Windows gets this scoping for free from `Local\`.
#[cfg(target_os = "linux")]
pub fn another_instance_running() -> bool {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let name = format!("MulVie_SingleInstance_{user}_{display}");
    let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) else {
        return false; // can't tell — behave as the primary instance
    };
    match UnixListener::bind_addr(&addr) {
        Ok(listener) => {
            std::mem::forget(listener); // hold the claim for our lifetime
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => true,
        Err(_) => false,
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn another_instance_running() -> bool {
    false
}

/// Ask Windows to stay awake while media is playing, via `SetThreadExecutionState`
/// on the CALLING thread (we call it from the eframe update loop, which runs on
/// the main thread — the request is thread-scoped). `system` keeps the machine
/// from sleeping; `display` additionally keeps the monitor on (no dim / no
/// blank). Both `false` releases the request, restoring the user's normal idle
/// timers. `ES_CONTINUOUS` makes the state persist until the next call, so this
/// only needs to run when the desired state changes.
#[cfg(windows)]
pub fn set_keep_awake(system: bool, display: bool, _window: Option<isize>) {
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    let mut flags = ES_CONTINUOUS;
    if system {
        flags |= ES_SYSTEM_REQUIRED;
    }
    if display {
        flags |= ES_DISPLAY_REQUIRED;
    }
    unsafe {
        let Ok(lib) = libloading::Library::new("kernel32.dll") else {
            return;
        };
        if let Ok(f) =
            lib.get::<unsafe extern "system" fn(u32) -> u32>(b"SetThreadExecutionState\0")
        {
            f(flags);
        }
    }
}

/// Linux: two best-effort child-process mechanisms, both writing nothing to
/// the host. `system` holds a blocking `systemd-inhibit … sleep infinity`
/// child (killed to release; released in on_exit too so it can't outlive
/// the app). `display` suspends the screensaver for OUR window via
/// `xdg-screensaver suspend <xid>` — scoped to the window, auto-cancelled by
/// the session if the window goes away. Only called on state CHANGES.
#[cfg(target_os = "linux")]
pub fn set_keep_awake(system: bool, display: bool, window: Option<isize>) {
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    struct Awake {
        inhibit: Option<Child>,
        display_on: bool,
        /// Screensaver commands run on ONE worker thread, in order — two
        /// detached threads racing (fast play→pause) could otherwise land
        /// suspend/resume reversed and leave the screensaver wrongly held.
        saver_tx: Option<std::sync::mpsc::Sender<(&'static str, String)>>,
    }
    static STATE: OnceLock<Mutex<Awake>> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        Mutex::new(Awake {
            inhibit: None,
            display_on: false,
            saver_tx: None,
        })
    });
    let Ok(mut s) = state.lock() else { return };

    if system {
        // A held child that already died (e.g. polkit denied the inhibit —
        // it does for session-less processes) must not masquerade as an
        // active inhibition: reap it so the next state change retries.
        if let Some(child) = &mut s.inhibit {
            if matches!(child.try_wait(), Ok(Some(_))) {
                s.inhibit = None;
            }
        }
    }
    if system && s.inhibit.is_none() {
        // The inhibited command is a WATCHDOG on our own pid, not `sleep
        // infinity`: if MulVie dies without cleanup (crash, kill -9, X server
        // gone), the loop exits within ~5s and the inhibitor is released —
        // a leaked block-mode inhibitor would otherwise stop the machine
        // from ever sleeping again. It also watches its own parent
        // (systemd-inhibit), so our kill() on release can't leave it behind.
        let watchdog = format!(
            "p=$PPID; while kill -0 {} 2>/dev/null && kill -0 $p 2>/dev/null; do sleep 5; done",
            std::process::id()
        );
        s.inhibit = Command::new("systemd-inhibit")
            .args([
                "--what=sleep:idle",
                "--who=MulVie",
                "--why=Media is playing",
                "--mode=block",
                "sh",
                "-c",
                &watchdog,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    } else if !system {
        if let Some(mut child) = s.inhibit.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    if let Some(xid) = window {
        if display != s.display_on {
            s.display_on = display;
            let verb = if display { "suspend" } else { "resume" };
            let arg = format!("0x{xid:x}");
            // Lazily start the single worker (off the UI thread — the tool is
            // a slow shell script); the channel preserves command order.
            let tx = s.saver_tx.get_or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel::<(&'static str, String)>();
                std::thread::spawn(move || {
                    for (verb, arg) in rx {
                        let _ = Command::new("xdg-screensaver")
                            .args([verb, &arg])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                    }
                });
                tx
            });
            let _ = tx.send((verb, arg));
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_keep_awake(_system: bool, _display: bool, _window: Option<isize>) {}
