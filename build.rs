// Point the linker at the bundled libmpv import library (libmpv.dll.a).
// The matching libmpv-2.dll must sit next to the built executable at runtime.
// The search dir is derived from CARGO_MANIFEST_DIR so it follows the repo if
// the folder is renamed/moved (Cargo caches this output — a repo move needs a
// build-script re-run, which any edit to this file forces).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={dir}/libmpv");

    // Installer payload: gzip the ASSEMBLED dist files into OUT_DIR so
    // src/bin/installer.rs can embed them. Only runs with the `installer`
    // feature, which package.ps1 enables AFTER assembling dist\MulVie —
    // a plain `cargo build` never touches this.
    if std::env::var_os("CARGO_FEATURE_INSTALLER").is_some() {
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let dist = std::path::Path::new(&dir).join("dist").join("MulVie");
        for name in [
            "mulvie.exe",
            "libmpv-2.dll",
            "pdfium.dll",
            "README.txt",
            "LICENSE",
            "THIRD-PARTY-LICENSES.txt",
        ] {
            let src = dist.join(name);
            println!("cargo:rerun-if-changed={}", src.display());
            let data = std::fs::read(&src).unwrap_or_else(|e| {
                panic!("installer payload {name} missing — run package.ps1 first: {e}")
            });
            let mut enc =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            std::io::Write::write_all(&mut enc, &data).unwrap();
            std::fs::write(out.join(format!("{name}.gz")), enc.finish().unwrap()).unwrap();
        }
    }

    // Embed the app icon into the exe so Explorer/desktop shows the blue "M"
    // instead of the default Windows icon. Uses windres (MinGW) on the GNU
    // toolchain — build via build.ps1, which puts MinGW on PATH.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "MulVie");
        res.set("FileDescription", "MulVie — portable multi-viewer");
        if let Err(e) = res.compile() {
            // Don't fail the whole build over an icon; just make it visible.
            println!("cargo:warning=icon embedding failed: {e}");
        }
    }
}
