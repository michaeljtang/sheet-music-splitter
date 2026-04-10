use std::process::Command;
use std::{env, fs};

const PDFIUM_RELEASE: &str = "chromium/7776";
const PDFIUM_BASE_URL: &str =
    "https://github.com/bblanchon/pdfium-binaries/releases/download";

fn main() {
    // We place libpdfium next to the binary (OUT_DIR's ancestor: target/<profile>/)
    let out_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_name = pdfium_lib_name();
    let dest = format!("{}/{}", out_dir, lib_name);

    if !fs::metadata(&dest).is_ok() {
        eprintln!("build.rs: downloading pdfium ({})...", lib_name);
        download_pdfium(&dest);
        eprintln!("build.rs: pdfium ready at {}", dest);
    }

    // Tell cargo where to find the dylib at link time (not needed for runtime
    // dynamic loading, but keeps linker happy if anything links it statically).
    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", dest);
}

fn pdfium_lib_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else {
        "libpdfium.so"
    }
}

fn pdfium_asset_name() -> String {
    // Pick the right archive from bblanchon/pdfium-binaries
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "pdfium-mac-arm64.tgz".to_string()
        } else {
            "pdfium-mac-x64.tgz".to_string()
        }
    } else if cfg!(target_os = "windows") {
        if cfg!(target_arch = "x86_64") {
            "pdfium-win-x64.tgz".to_string()
        } else {
            "pdfium-win-x86.tgz".to_string()
        }
    } else {
        // Linux
        if cfg!(target_arch = "aarch64") {
            "pdfium-linux-arm64.tgz".to_string()
        } else {
            "pdfium-linux-x64.tgz".to_string()
        }
    }
}

fn download_pdfium(dest: &str) {
    let asset = pdfium_asset_name();
    let url = format!("{}/{}/{}", PDFIUM_BASE_URL, PDFIUM_RELEASE, asset);
    let tgz = format!("{}.tgz", dest);

    // Download
    let status = Command::new("curl")
        .args(["-L", "--fail", "--silent", "--show-error", "-o", &tgz, &url])
        .status()
        .expect("curl not found — install curl to auto-download pdfium");
    assert!(status.success(), "Failed to download pdfium from {}", url);

    // Extract just the library file from the archive
    let lib_name = pdfium_lib_name();
    let out_dir = dest.rsplitn(2, '/').last().unwrap();
    // Archive structure: lib/libpdfium.dylib — extract with strip-components=1
    let status = Command::new("tar")
        .args([
            "-xzf", &tgz,
            "-C", out_dir,
            "--strip-components=1",
            &format!("lib/{}", lib_name),
        ])
        .status()
        .expect("tar not found");

    if !status.success() {
        panic!("tar extraction failed for {}", tgz);
    }

    fs::remove_file(&tgz).ok();

    assert!(
        fs::metadata(dest).is_ok(),
        "pdfium extraction failed — {} not found after extracting {}",
        dest, asset
    );
}
