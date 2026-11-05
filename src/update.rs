//! Automatic updates from GitHub Releases.
//!
//! - Checks the latest (non-pre-release) release of the repository.
//! - Downloads the asset for this platform over HTTPS and verifies it
//!   against the release's SHA256SUMS.txt before using it.
//! - Installed copies (Windows installer) are updated by running the new
//!   installer silently when the app closes or restarts, which keeps the
//!   uninstaller and "Apps" list in step.
//! - Portable copies replace their own executable; the new version starts
//!   the next time the app is opened.

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REPO: &str = "MattGodwin/mail-rs";
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize, Clone, Debug)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub url: String,
    assets: Vec<GhAsset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Installed with the Windows installer: update by running the new installer.
    Installer,
    /// A loose executable: update by replacing it.
    Portable,
}

#[derive(Clone, Debug)]
pub struct Staged {
    pub version: Version,
    pub path: PathBuf,
    pub mode: Mode,
    pub url: String,
}

/// Progress messages from the background update thread.
pub enum Msg {
    UpToDate,
    Available(Release),
    Downloading(Version),
    Ready(Staged),
    Failed(String),
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}

/// Release asset key for this build, or None if releases don't cover it.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("macos-x64")
    } else {
        None
    }
}

/// Name of the plain executable asset for a platform.
pub fn binary_asset(platform: &str) -> String {
    let ext = if platform.starts_with("windows") { ".exe" } else { "" };
    format!("mail-rs-{platform}{ext}")
}

pub const INSTALLER_ASSET: &str = "mail-rs-setup-windows-x64.exe";

pub fn mode() -> Mode {
    let installed = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("unins000.exe").exists())).unwrap_or(false);
    if cfg!(windows) && installed {
        Mode::Installer
    } else {
        Mode::Portable
    }
}

fn agent() -> Result<ureq::Agent, String> {
    Ok(crate::http::agent(Duration::from_secs(10 * 60)))
}

fn get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = agent.get(url).header("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string())?;
    crate::http::ensure_ok(&mut resp, url)?;
    resp.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(|e| e.to_string())
}

/// The newest published (non-draft, non-pre-release) release.
pub fn latest() -> Result<Release, String> {
    let agent = agent()?;
    let body = get(&agent, &format!("https://api.github.com/repos/{REPO}/releases/latest")).map_err(|e| {
        if e.contains("HTTP 404") {
            "No releases have been published yet".to_string()
        } else {
            e
        }
    })?;
    let r: GhRelease = serde_json::from_slice(&body).map_err(|e| format!("Unexpected response from GitHub: {e}"))?;
    if r.draft || r.prerelease {
        return Err("Latest release is not a stable release".into());
    }
    let version =
        Version::parse(r.tag_name.trim_start_matches('v')).map_err(|e| format!("Release tag {} is not a version: {e}", r.tag_name))?;
    Ok(Release { version, url: r.html_url, assets: r.assets })
}

/// A newer release than the running one, if any.
pub fn check() -> Result<Option<Release>, String> {
    let r = latest()?;
    Ok((r.version > current()).then_some(r))
}

fn updates_dir() -> PathBuf {
    crate::config::cache_dir().join("updates")
}

pub fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download and verify the update for this platform and install mode.
pub fn download(r: &Release) -> Result<Staged, String> {
    let platform = platform().ok_or("Updates aren't published for this platform")?;
    let mode = mode();
    let name = match mode {
        Mode::Installer => INSTALLER_ASSET.to_string(),
        Mode::Portable => binary_asset(platform),
    };
    let find = |n: &str| r.assets.iter().find(|a| a.name == n).cloned();
    let asset = find(&name).ok_or_else(|| format!("Release {} has no {name}", r.version))?;
    let sums_asset = find("SHA256SUMS.txt").ok_or("Release has no SHA256SUMS.txt; refusing an unverifiable update")?;
    let agent = agent()?;
    let sums = String::from_utf8(get(&agent, &sums_asset.browser_download_url)?).map_err(|_| "SHA256SUMS.txt is not text")?;
    let expected = expected_hash(&sums, &name).ok_or_else(|| format!("No checksum listed for {name}"))?;
    let data = get(&agent, &asset.browser_download_url)?;
    let actual = hex(&Sha256::digest(&data));
    if actual != expected {
        return Err(format!("Checksum mismatch for {name}; the download was discarded"));
    }
    let dir = updates_dir().join(r.version.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&name);
    std::fs::write(&path, &data).map_err(|e| format!("Saving update: {e}"))?;
    make_executable(&path)?;
    Ok(Staged { version: r.version.clone(), path, mode, url: r.url.clone() })
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Replace the running executable with the staged one. The running process
/// keeps working (it was moved aside); the next launch uses the new version.
pub fn apply_portable(s: &Staged) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_path(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("Couldn't update {} (is it in a read-only folder?): {e}", exe.display()))?;
    if let Err(e) = std::fs::copy(&s.path, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Couldn't write the new version: {e}"));
    }
    make_executable(&exe)
}

/// Run the staged installer silently. `relaunch` reopens Mail afterwards.
pub fn run_installer(s: &Staged, relaunch: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new(&s.path);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"]);
    if !relaunch {
        cmd.arg("/NORELAUNCH");
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("Couldn't start the installer: {e}"))
}

/// Start a fresh copy of the (possibly just replaced) executable.
pub fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // After a portable swap, current_exe may point at the renamed old file.
    let exe = if exe.extension().and_then(|e| e.to_str()) == Some("old") { exe.with_extension("") } else { exe };
    std::process::Command::new(exe).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Remove leftovers from previous updates.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
    let _ = std::fs::remove_dir_all(updates_dir());
}

/// Check, and (when `install` is set) download and stage, on a background thread.
pub fn spawn(install: bool, send: impl Fn(Msg) + Send + 'static) {
    std::thread::spawn(move || {
        let r = match check() {
            Ok(Some(r)) => r,
            Ok(None) => return send(Msg::UpToDate),
            Err(e) => return send(Msg::Failed(e)),
        };
        if !install {
            return send(Msg::Available(r));
        }
        send(Msg::Downloading(r.version.clone()));
        let staged = match download(&r) {
            Ok(s) => s,
            Err(e) => return send(Msg::Failed(e)),
        };
        if staged.mode == Mode::Portable {
            if let Err(e) = apply_portable(&staged) {
                return send(Msg::Failed(e));
            }
        }
        send(Msg::Ready(staged));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_checksums() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  mail-rs-linux-x64\n\
                    ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789 *mail-rs-windows-x64.exe\n";
        assert_eq!(expected_hash(sums, "mail-rs-linux-x64").unwrap().len(), 64);
        assert!(expected_hash(sums, "mail-rs-windows-x64.exe").unwrap().starts_with("abcdef"));
        assert!(expected_hash(sums, "other").is_none());
    }

    #[test]
    fn asset_names() {
        assert_eq!(binary_asset("windows-x64"), "mail-rs-windows-x64.exe");
        assert_eq!(binary_asset("macos-arm64"), "mail-rs-macos-arm64");
        assert!(platform().is_some());
    }

    #[test]
    fn parses_release_json() {
        let json = r#"{"tag_name":"v9.1.0","html_url":"https://x","draft":false,"prerelease":false,
            "assets":[{"name":"SHA256SUMS.txt","browser_download_url":"https://y"}]}"#;
        let r: GhRelease = serde_json::from_str(json).unwrap();
        assert_eq!(Version::parse(r.tag_name.trim_start_matches('v')).unwrap(), Version::new(9, 1, 0));
        assert!(Version::new(9, 1, 0) > current());
    }

    /// Live check against GitHub (network): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_latest_release() {
        match latest() {
            Ok(r) => assert!(r.assets.iter().any(|a| a.name == "SHA256SUMS.txt"), "release lacks checksums"),
            Err(e) => assert!(e.contains("No releases"), "unexpected error: {e}"),
        }
    }
}
