// MulVie one-file "installer": a single exe that unpacks the portable MulVie
// folder next to itself, for easy sharing. It is a plain self-extractor —
// no registry, no system files, no admin rights: it only creates .\MulVie\
// and writes the app files into it. The installer exe itself is left
// untouched, so it can be kept and passed on.
//
// Built by package.ps1 (`cargo build --release --features installer`) AFTER
// the dist folder is assembled; build.rs gzips the payload into OUT_DIR.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    match unpack() {
        Ok(dir) => {
            let msg = format!(
                "MulVie {} was unpacked to:\n{}\n\nThe folder is fully portable - move or copy it \
                 anywhere, including a USB stick. This installer file is no longer needed, but \
                 you can keep it to share MulVie with others.\n\nLaunch MulVie now?",
                env!("CARGO_PKG_VERSION"),
                dir.display()
            );
            if message_box(&msg, MB_YESNO | MB_ICONINFORMATION) == IDYES {
                // Launch from the folder so config/libraries land next to the app.
                let _ = std::process::Command::new(dir.join("mulvie.exe"))
                    .current_dir(&dir)
                    .spawn();
            }
        }
        Err(e) => {
            message_box(
                &format!(
                    "Unpacking failed:\n{e}\n\nIf this location is write-protected, copy the \
                     installer into a writable folder and run it there."
                ),
                MB_ICONERROR,
            );
        }
    }
}

/// Decompress every payload file into a `MulVie` folder next to this exe.
#[cfg(windows)]
fn unpack() -> Result<std::path::PathBuf, String> {
    use std::io::Read;

    // (name, gzipped bytes) — compressed at build time by build.rs.
    const FILES: [(&str, &[u8]); 6] = [
        ("mulvie.exe", include_bytes!(concat!(env!("OUT_DIR"), "/mulvie.exe.gz"))),
        ("libmpv-2.dll", include_bytes!(concat!(env!("OUT_DIR"), "/libmpv-2.dll.gz"))),
        ("pdfium.dll", include_bytes!(concat!(env!("OUT_DIR"), "/pdfium.dll.gz"))),
        ("README.txt", include_bytes!(concat!(env!("OUT_DIR"), "/README.txt.gz"))),
        ("LICENSE", include_bytes!(concat!(env!("OUT_DIR"), "/LICENSE.gz"))),
        (
            "THIRD-PARTY-LICENSES.txt",
            include_bytes!(concat!(env!("OUT_DIR"), "/THIRD-PARTY-LICENSES.txt.gz")),
        ),
    ];

    let dir = std::env::current_exe()
        .map_err(|e| format!("own path unknown: {e}"))?
        .parent()
        .ok_or("no parent folder")?
        .join("MulVie");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for (name, gz) in FILES {
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_end(&mut data)
            .map_err(|e| format!("decompress {name}: {e}"))?;
        std::fs::write(dir.join(name), &data)
            .map_err(|e| format!("write {name}: {e}"))?;
    }
    Ok(dir)
}

#[cfg(windows)]
const MB_YESNO: u32 = 0x0004;
#[cfg(windows)]
const MB_ICONERROR: u32 = 0x0010;
#[cfg(windows)]
const MB_ICONINFORMATION: u32 = 0x0040;
#[cfg(windows)]
const IDYES: i32 = 6;

#[cfg(windows)]
fn message_box(text: &str, flags: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let text = wide(text);
    let title = wide("MulVie");
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}

#[cfg(not(windows))]
fn main() {
    // The one-file installer is a Windows convenience; Linux users build from
    // source (see README) or receive the plain folder.
    eprintln!("The MulVie installer is Windows-only.");
}
