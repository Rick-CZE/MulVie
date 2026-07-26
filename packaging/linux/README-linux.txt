MulVie - Linux x86-64
=====================

Requirements
  - An x86-64 Linux desktop with X11 (tested on Linux Mint 22).
  - libmpv for video/audio: preinstalled on Linux Mint; elsewhere install
    it with your package manager (e.g. sudo apt install libmpv2).

Run
  Unpack this folder anywhere - a USB stick works too - and start:
      ./mulvie
  All settings stay inside this folder; nothing is written to the host.
  PDF support works out of the box (libpdfium.so ships next to the binary).

Desktop launcher (optional)
  MulVie.desktop is a template for a clickable launcher with the app icon.
  Open it in a text editor and replace both /path/to/MulVie with the real
  absolute path of this folder, then make it executable and copy it where
  you want it:
      chmod +x MulVie.desktop
      cp MulVie.desktop ~/Desktop/

Manual: see README.txt.
License: MIT (see LICENSE). The bundled libpdfium.so is PDFium
(BSD-3-Clause with Apache-2.0 components); libmpv is provided by your
system and keeps its own license.
