===

This application has been created by Rick-CZE with AI assistance
(Anthropic - Claude Opus 4.8 and Claude Fable 5), but it is not
vibe-coded low-effort "slopware".

I've created this app for myself - to be used, and I love using it.
I think someone else might find it useful, too.

===

# MulVie - multi-viewer

A **portable** presentation multi-viewer for **Windows 10 & 11 and Linux**
(tested on Linux Mint 22, X11): one window, up to **four panels** (draggable
dividers), each browsing its own folder of **images, animated GIFs, video,
audio, or PDFs**.

Built for running straight off a **USB stick** on any PC: no installation,
no internet, and nothing written to the host disk by its own code — all
settings live in the app directory. Content is read, shown, and gone when
you unplug.

MulVie can also be set as the default picture or video application on
Windows - a lightweight substitute for the resource-heavy but feature-poor
options that come pre-installed. Free & open source; if you like it,
consider sharing it.

## Feature overview (v1.0)

- **Panels:** 1–4 via draggable dividers (drag to an edge to collapse); single ↔
  four-panel toggle (G); per-panel zoom/pan/rotate; lock a panel against bulk
  actions; drag & drop files or folders onto any panel.
- **Media:** images (JPG, PNG, GIF animated, BMP, TIFF, WebP, TGA, QOI), video
  + audio via bundled libmpv (tracks, subtitles, A-B loop, speed, per-panel
  volume/mute), PDF via bundled pdfium. Folders load recursively.
- **Gallery management:** up to 4 browser instances (standalone windows or
  pinned into a panel) — thumbnails, marks, search, type filter, sort; file
  mode with batch rename, find-duplicates, delete (recycle bin) and move.
- **Presentation:** frameless silver/dark-blue chrome, fullscreen (F11),
  presentation cover (Shift+H) with optional freeze, cursor auto-hide,
  keep-awake while playing, background/text tint with acrylic glass.
- **Library:** save/load named panel layouts (paths only, stored next to the
  exe on the stick).
- The full user manual ships as `README.txt` beside the exe (`dist\MulVie`).

## Build (Windows, GNU toolchain)

Requires rustup with the `x86_64-pc-windows-gnu` toolchain plus a full
MinGW-w64 on PATH (see `build.ps1`, which sets this up).

```powershell
.\build.ps1                # debug build
.\build.ps1 --release      # optimized build
.\package.ps1              # assemble the portable folder -> dist\MulVie
```

`dist\MulVie` is the whole app: `mulvie.exe` + `libmpv-2.dll` + `pdfium.dll` +
`README.txt` (~126 MB). Copy the folder to a stick and run.

`package.ps1` also produces `dist\MulVie-<version>-setup.exe` — a single
self-unpacking file for easy sharing. Running it simply creates a `MulVie`
folder next to itself with the four files above (no real installation: no
registry, no system files, no admin rights) and offers to launch the app;
the setup file itself is left untouched so it can be passed on.

## Build (Linux)

The same source builds natively on Linux (tested on Linux Mint 22, X11).
Requires rustup and a system libmpv (`libmpv.so.2` ships with Mint;
elsewhere `sudo apt install libmpv2`). Without `libmpv-dev`, point the
linker at the runtime library once:

```bash
mkdir -p ~/lib && ln -s /usr/lib/x86_64-linux-gnu/libmpv.so.2 ~/lib/libmpv.so
RUSTFLAGS="-L $HOME/lib" cargo build --release
```

For binaries you intend to publish, add `--remap-path-prefix=$HOME=~` to
`RUSTFLAGS` so no local paths end up embedded in the executable (the
Windows `build.ps1` does the equivalent automatically).

For PDF support, place a `libpdfium.so` (e.g. from
[pdfium-binaries](https://github.com/bblanchon/pdfium-binaries), the
`lib/` folder of `pdfium-linux-x64.tgz`) next to the built
`target/release/mulvie`. Without it, everything except PDF viewing works.
A sample `.desktop` launcher (icon + double-click start) is described in
the user manual.

## Privacy

The app's own code writes only `mulvie_config.json` (settings) and
`mulvie_libraries.json` (saved layouts, paths only) — both next to the exe, on
the stick — plus a transient `mulvie_open.txt` handoff file. Nothing goes to
the host PC.

## License

MulVie's source code is MIT-licensed (see [LICENSE](LICENSE)).

Release bundles additionally ship third-party libraries that keep their own
licenses: [mpv](https://mpv.io) (`libmpv-2.dll` — GPL/LGPL, sources at
mpv.io; on Linux the system libmpv is used) and
[PDFium](https://pdfium.googlesource.com/pdfium/) (`pdfium.dll` /
`libpdfium.so` — BSD-3-Clause with Apache-2.0 components). Every binary
bundle includes `THIRD-PARTY-LICENSES.txt` with the notices and full
license texts. This repository itself contains no third-party binaries.
