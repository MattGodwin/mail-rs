//! Embeds the app icon and version metadata into the Windows executable.
//! For GNU-toolchain Windows builds, also places WebView2Loader.dll next to
//! the executable (MSVC builds, used for releases, link the loader statically).
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "Mail");
    res.set("FileDescription", "Mail");
    res.set("CompanyName", "Matthew Godwin");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the Windows icon: {e}");
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        copy_webview2_loader();
    }
}

fn copy_webview2_loader() {
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => return,
    };
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".cargo"));
    let Ok(regs) = std::fs::read_dir(cargo_home.join("registry").join("src")) else { return };
    let mut found: Vec<PathBuf> = regs
        .flatten()
        .flat_map(|r| std::fs::read_dir(r.path()).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("webview2-com-sys-")))
        .map(|p| p.join(arch).join("WebView2Loader.dll"))
        .filter(|p| p.exists())
        .collect();
    found.sort();
    let (Some(src), Ok(out)) = (found.pop(), std::env::var("OUT_DIR")) else { return };
    // OUT_DIR is target/<profile>/build/<pkg>/out; the executable lives in target/<profile>.
    if let Some(profile_dir) = PathBuf::from(out).ancestors().nth(3) {
        let _ = std::fs::copy(&src, profile_dir.join("WebView2Loader.dll"));
    }
}
