//! Framework package manager.
//!
//! A project declares the Koral framework version it builds against. On any machine, the
//! Hub resolves that version to a prebuilt, per-platform SDK under
//! `<data>/frameworks/<version>/<platform>/`, downloading and unpacking it on demand. This
//! is what turns "clone the repo" into "clone and run" regardless of OS.
//!
//! Each installed SDK carries a `framework.json` manifest describing the ABI it was built
//! with (vcpkg baseline), where its CMake package config lives, and the runtime executable
//! that loads project scenes. The Hub reads the baseline from there rather than hardcoding
//! it, so Hub and framework releases can version independently.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// Base URL that publishes SDK release archives. Convention:
/// `<base>/v<version>/koral-sdk-<version>-<platform>.zip`. Configurable via settings later.
const REGISTRY_BASE: &str = "https://github.com/radue/koral/releases/download";

/// Manifest shipped inside every installed SDK (`framework.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameworkManifest {
    pub name: String,
    pub version: String,
    pub platform: String,
    /// vcpkg baseline the SDK's public headers were compiled against. Generated projects
    /// must resolve their ports at this same baseline to stay ABI-compatible.
    pub vcpkg_baseline: String,
    /// Path, relative to the SDK root, of the runtime executable that loads scenes.
    pub runtime: String,
    /// Path, relative to the SDK root, of the CMake package-config directory to hand to
    /// consumers (via `CMAKE_PREFIX_PATH`) when configuring a project.
    pub cmake_dir: String,
}

/// A framework SDK present on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledFramework {
    pub version: String,
    pub platform: String,
    pub path: String,
}

/// Host platform tag used in SDK archive names, e.g. `linux-x86_64`, `windows-x86_64`,
/// `macos-aarch64`.
pub fn host_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        other => other, // "windows", "linux"
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn install_dir(version: &str, platform: &str) -> PathBuf {
    paths::frameworks_dir().join(version).join(platform)
}

fn archive_url(version: &str, platform: &str) -> String {
    format!("{REGISTRY_BASE}/v{version}/koral-sdk-{version}-{platform}.zip")
}

pub fn read_manifest(sdk_root: &Path) -> Result<FrameworkManifest, String> {
    let file = sdk_root.join("framework.json");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", file.display()))
}

/// Every SDK installed on this machine.
pub fn installed() -> Vec<InstalledFramework> {
    let mut out = Vec::new();
    let Ok(versions) = std::fs::read_dir(paths::frameworks_dir()) else {
        return out;
    };
    for version in versions.flatten() {
        if !version.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let version_name = version.file_name().to_string_lossy().into_owned();
        let Ok(platforms) = std::fs::read_dir(version.path()) else {
            continue;
        };
        for platform in platforms.flatten() {
            let dir = platform.path();
            if !dir.join("framework.json").exists() {
                continue;
            }
            out.push(InstalledFramework {
                version: version_name.clone(),
                platform: platform.file_name().to_string_lossy().into_owned(),
                path: dir.to_string_lossy().into_owned(),
            });
        }
    }
    out
}

/// Ensure the requested framework version is installed for this platform, downloading it
/// if necessary, and return its SDK root. Blocking — Tauri runs sync commands off the UI
/// thread, so this won't freeze the webview.
pub fn ensure_installed(version: &str) -> Result<PathBuf, String> {
    let platform = host_platform();
    let dir = install_dir(version, &platform);

    if !dir.join("framework.json").exists() {
        download_and_extract(version, &platform, &dir)?;
    }

    let manifest = read_manifest(&dir)?;
    if manifest.version != version {
        return Err(format!(
            "installed SDK reports version {} but {version} was requested",
            manifest.version
        ));
    }
    Ok(dir)
}

/// Download the SDK archive and unpack it into `dest`. Extracts into a sibling temp dir
/// first and moves it into place only on success, so a failed download never leaves a
/// half-installed SDK that looks valid.
fn download_and_extract(version: &str, platform: &str, dest: &Path) -> Result<(), String> {
    let url = archive_url(version, platform);

    let bytes = reqwest::blocking::get(url.as_str())
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("failed to download SDK from {url}: {e}"))?;

    let staging = dest.with_extension("downloading");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("invalid SDK archive: {e}"))?;
    archive
        .extract(&staging)
        .map_err(|e| format!("failed to unpack SDK: {e}"))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&staging, dest).map_err(|e| format!("failed to install SDK: {e}"))?;
    Ok(())
}
