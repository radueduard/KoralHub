//! Framework package manager.
//!
//! A project declares the Koral framework version it builds against. On any machine, the
//! Hub resolves that version to a prebuilt, per-platform SDK under
//! `<data>/frameworks/<version>/<platform>/`, downloading and unpacking it on demand. This
//! is what turns "clone the repo" into "clone and run" regardless of OS.
//!
//! Releases are discovered through the GitHub releases API rather than by guessing URLs.
//! That matters for one specific reason: the framework repository is mid-rename (GFX_RELOADED
//! -> Koral), so its release assets are called `gfx-sdk-*` today and will be `koral-sdk-*`
//! tomorrow. Matching assets by *shape* (`<anything>-sdk-<version>-<platform>.<ext>`) instead
//! of by a hardcoded name means the rename does not strand the Hub, and old versions stay
//! installable afterwards.
//!
//! Each installed SDK carries a `framework.json` manifest describing where its CMake package
//! config lives and which executable runs project scenes. The published SDK does not ship one
//! yet, so [`write_manifest`] synthesises it by inspecting the unpacked tree. If a future
//! release includes its own `framework.json`, it is left alone and wins.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::paths;

/// The framework's GitHub repository. Renaming it on GitHub leaves a redirect behind, and the
/// API follows redirects, so this keeps working after the rename — but update it anyway.
const REPO: &str = "radueduard/Koral";

/// GitHub requires a User-Agent on every API request and 403s without one.
const USER_AGENT: &str = "KoralHub";

/// Manifest describing an installed SDK (`framework.json` at the SDK root).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameworkManifest {
    pub name: String,
    pub version: String,
    pub platform: String,
    /// Path, relative to the SDK root, of the runtime executable that loads scenes.
    pub runtime: String,
    /// Path, relative to the SDK root, of the CMake package-config directory to hand to
    /// consumers (via `CMAKE_PREFIX_PATH`) when configuring a project.
    pub cmake_dir: String,
    /// vcpkg baseline the SDK's public headers were built against, if it declares one.
    ///
    /// Empty for current releases, and that is not a bug: the SDK *vendors* the dependencies
    /// that leak through its public headers (glm, spdlog, imgui) as static libraries under
    /// `lib/gfx-vendor`, so a consuming project links them straight out of the SDK and needs
    /// no vcpkg at all. This field exists for the day a project wants extra ports of its own
    /// and we need an ABI-compatible baseline to resolve them at.
    #[serde(default)]
    pub vcpkg_baseline: String,
}

/// A framework SDK present on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledFramework {
    pub version: String,
    pub platform: String,
    pub path: String,
    /// Bytes on disk. Shown in the UI so it is obvious what uninstalling reclaims.
    pub size_bytes: u64,
}

/// A release that *could* be installed on this machine — i.e. one that publishes an SDK
/// asset for the host platform. Releases with no matching asset are dropped, since offering
/// to install something that cannot be downloaded is worse than not listing it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableFramework {
    pub version: String,
    pub tag: String,
    pub published_at: String,
    pub prerelease: bool,
    /// An unpublished release, visible only because this machine has a GitHub token. Surfaced
    /// so the UI can say so — nobody else can install this, so it must not look normal.
    pub draft: bool,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    /// Whether this exact version is already unpacked for the host platform.
    pub installed: bool,
}

// --- GitHub releases API ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    /// Null for drafts — they have never been published — so this cannot be a bare String.
    #[serde(default)]
    published_at: Option<String>,
    /// Always set, and the only date a draft has. Used to order drafts sensibly.
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

impl GhRelease {
    /// The date to sort on: when it went public, or failing that when it was created.
    fn date(&self) -> String {
        self.published_at
            .clone()
            .or_else(|| self.created_at.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    /// Public CDN link. 404s for a draft's assets, even with a token.
    browser_download_url: String,
    /// API link (`.../releases/assets/<id>`). The only way to fetch a draft's asset, and it
    /// needs both a token and `Accept: application/octet-stream`.
    url: String,
    size: u64,
}

/// A token for the GitHub API, if this machine has one: `$GITHUB_TOKEN`/`$GH_TOKEN`, else
/// whatever the `gh` CLI is logged in with.
///
/// Only needed to see *draft* releases (see [`available`]). Without a token the Hub is an
/// anonymous client and behaves exactly as an end user's would.
fn github_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
                if let Ok(t) = std::env::var(var) {
                    if !t.trim().is_empty() {
                        return Some(t.trim().to_string());
                    }
                }
            }
            // The Hub is usually launched from a desktop shortcut with no environment to
            // speak of, so the env vars alone would almost never hit. Ask `gh` instead.
            let out = std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let t = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (!t.is_empty()).then_some(t)
        })
        .as_deref()
}

fn http() -> Result<reqwest::blocking::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = github_token() {
        // reqwest drops Authorization when a redirect crosses hosts, so following an asset
        // download out to GitHub's storage CDN will not leak the token.
        if let Ok(mut v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            v.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))
}

/// Host platform tag as it appears in release asset names (`linux-x64`, `windows-x64`,
/// `macos-arm64`).
///
/// This must track the `name:` values in the framework's release matrix, which uses the short
/// `x64`/`arm64` spelling — NOT Rust's `std::env::consts::ARCH`, which says `x86_64`. Getting
/// this wrong means every asset silently fails to match and the Hub reports "no release for
/// this platform" against a release that plainly has one.
pub fn host_platform() -> String {
    let os = std::env::consts::OS; // "linux" | "windows" | "macos"
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Does this asset carry the SDK for `platform`?
///
/// Deliberately matches on shape rather than an exact name, so the pending `gfx-*` -> `koral-*`
/// rename does not break discovery. Requires the `-sdk-` infix specifically: every release also
/// publishes a `*-runtime-*` archive, which is a strict subset (no headers, no CMake package)
/// and would produce an SDK that cannot be built against.
fn is_sdk_asset(name: &str, platform: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = match lower.strip_suffix(".tar.gz").or_else(|| lower.strip_suffix(".zip")) {
        Some(s) => s,
        None => return false,
    };
    stem.contains("-sdk-") && stem.ends_with(&format!("-{platform}"))
}

/// Every release that publishes an SDK for this platform, newest first.
pub fn available() -> Result<Vec<AvailableFramework>, String> {
    let platform = host_platform();
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=50");

    let releases: Vec<GhRelease> = http()?
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("failed to list releases from {REPO}: {e}"))?;

    let installed_versions: Vec<String> = installed()
        .into_iter()
        .filter(|f| f.platform == platform)
        .map(|f| f.version)
        .collect();

    // A draft's assets 404 for anonymous downloads, so listing one without a token would just
    // produce an install button that always fails. With a token we can both see and fetch it,
    // which is what lets the framework be developed against before it is published.
    let authed = github_token().is_some();

    let mut out: Vec<AvailableFramework> = releases
        .into_iter()
        .filter(|r| authed || !r.draft)
        .filter_map(|r| {
            let asset = r.assets.iter().find(|a| is_sdk_asset(&a.name, &platform))?;
            let version = r.tag_name.trim_start_matches('v').to_string();
            let date = r.date();
            Some(AvailableFramework {
                installed: installed_versions.contains(&version),
                version,
                tag: r.tag_name,
                published_at: date,
                prerelease: r.prerelease,
                // A draft has no working CDN link; it can only be fetched through the API.
                asset_url: if r.draft {
                    asset.url.clone()
                } else {
                    asset.browser_download_url.clone()
                },
                draft: r.draft,
                asset_name: asset.name.clone(),
                asset_size: asset.size,
            })
        })
        .collect();

    out.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    Ok(out)
}

// --- Local installs ---------------------------------------------------------------------

/// Reject anything that could escape the frameworks directory once joined onto a path.
/// Versions come from release tags, which are attacker-influencable in principle and are
/// certainly typo-influencable in practice — and this string is about to be handed to
/// `remove_dir_all`.
fn validate_version(version: &str) -> Result<(), String> {
    let ok = !version.is_empty()
        && version != ".."
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if ok {
        Ok(())
    } else {
        Err(format!("refusing to use '{version}' as a version directory name"))
    }
}

fn install_dir(version: &str, platform: &str) -> PathBuf {
    paths::frameworks_dir().join(version).join(platform)
}

pub fn read_manifest(sdk_root: &Path) -> Result<FrameworkManifest, String> {
    let file = sdk_root.join("framework.json");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", file.display()))
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0, // symlinks: the target is counted where it actually lives
        })
        .sum()
}

/// Every SDK installed on this machine.
pub fn installed() -> Vec<InstalledFramework> {
    let mut out = Vec::new();
    let Ok(versions) = std::fs::read_dir(paths::frameworks_dir()) else {
        return out; // no frameworks dir yet — nothing installed, not an error
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
            // The manifest is written last, after the atomic rename, so its presence is what
            // makes an install "complete". A half-unpacked tree has no manifest and is ignored.
            if !dir.join("framework.json").exists() {
                continue;
            }
            out.push(InstalledFramework {
                version: version_name.clone(),
                platform: platform.file_name().to_string_lossy().into_owned(),
                size_bytes: dir_size(&dir),
                path: dir.to_string_lossy().into_owned(),
            });
        }
    }
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// Remove an installed SDK. Idempotent: uninstalling something that is not there succeeds.
pub fn uninstall(version: &str) -> Result<(), String> {
    validate_version(version)?;
    let platform = host_platform();
    let dir = install_dir(version, &platform);

    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("failed to remove {}: {e}", dir.display()))?;
    }

    // Drop the now-empty <version>/ parent so the frameworks dir does not fill up with husks.
    // Only if empty — another platform's SDK may still live under it.
    if let Some(parent) = dir.parent() {
        if parent.read_dir().map(|mut d| d.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(())
}

/// Download and unpack `version` for the host platform, reporting progress as
/// `(bytes_done, total_bytes)`. `total` is 0 when the server sends no Content-Length.
///
/// Unpacks into a sibling staging directory and moves it into place only once it is complete,
/// so an interrupted download can never leave behind a tree that looks installed.
pub fn install(version: &str, mut progress: impl FnMut(u64, u64)) -> Result<PathBuf, String> {
    validate_version(version)?;
    let platform = host_platform();

    let release = available()?
        .into_iter()
        .find(|r| r.version == version)
        .ok_or_else(|| {
            // The overwhelmingly likely cause while the framework is pre-1.0: the release
            // exists but is still a draft, and this machine has no token to see it with.
            if github_token().is_some() {
                format!("no release {version} publishes an SDK for {platform}")
            } else {
                format!(
                    "no release {version} publishes an SDK for {platform} — if it is still an \
                     unpublished draft, sign in with `gh auth login` (or set $GITHUB_TOKEN) so \
                     the Hub can see it"
                )
            }
        })?;

    let dest = install_dir(version, &platform);
    let staging = dest.with_extension("downloading");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("failed to create {}: {e}", staging.display()))?;

    // Stream to memory with progress. These archives are ~40 MB; holding one in RAM is fine
    // and avoids a temp file we would then have to clean up on every failure path.
    // Without this the asset API hands back the asset's JSON metadata instead of its bytes.
    // Harmless on the public CDN path, which ignores it.
    let mut response = http()?
        .get(&release.asset_url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("failed to download {}: {e}", release.asset_name))?;

    let total = response.content_length().unwrap_or(release.asset_size);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        progress(bytes.len() as u64, total);
    }

    let result = (|| -> Result<(), String> {
        if release.asset_name.ends_with(".zip") {
            extract_zip(&bytes, &staging)
        } else {
            extract_tar_gz(&bytes, &staging)
        }?;
        prefer_system_vulkan(&staging);
        write_manifest(&staging, version, &platform, &release.asset_name)
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&staging, &dest)
        .map_err(|e| format!("failed to install SDK into {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Ensure `version` is installed, downloading it if necessary, and return its SDK root.
/// The build path calls this; it is silent, with no progress reporting.
pub fn ensure_installed(version: &str) -> Result<PathBuf, String> {
    let dir = install_dir(version, &host_platform());
    if dir.join("framework.json").exists() {
        return Ok(dir);
    }
    install(version, |_, _| {})
}

// --- Unpacking --------------------------------------------------------------------------

/// Both archive kinds wrap everything in a single top-level directory named after the archive
/// (`koral-sdk-0.0.1-linux-x64/bin/...`). Strip it, so the SDK root *is* the install dir and
/// paths in `framework.json` stay relative to something stable.
///
/// Returns `None` for the wrapper directory itself and for any entry that is not a plain
/// relative path. Rejecting `..`/absolute components is what keeps an entry from escaping the
/// install directory — the callers join this onto `dest` and write there directly, so this is
/// the only containment check there is.
fn strip_root(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let mut parts = path.components();
    parts.next()?; // discard the wrapper directory

    let mut rest = PathBuf::new();
    for part in parts {
        match part {
            Component::Normal(c) => rest.push(c),
            Component::CurDir => {}
            // `..`, `/`, or a Windows prefix — nothing legitimate in an SDK archive.
            _ => return None,
        }
    }
    (!rest.as_os_str().is_empty()).then_some(rest)
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    // Preserve the executable bit on bin/* and the .so symlink chains (libvulkan.so.1 ->
    // libvulkan.so.1.4.335); without this the runtime unpacks non-executable and the loader
    // cannot follow the sonames.
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);

    for entry in archive
        .entries()
        .map_err(|e| format!("unreadable SDK archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("corrupt SDK archive: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("bad path in SDK archive: {e}"))?
            .into_owned();
        let Some(relative) = strip_root(&path) else {
            continue; // the wrapper directory, or an entry that refuses to sit under `dest`
        };
        let target = dest.join(&relative);

        // NOT `unpack_in`: that resolves the entry against its own archived path, which still
        // carries the wrapper directory we just stripped, and would nest the tree under it.
        // `unpack` writes exactly where it is told — `strip_root` is what keeps that in bounds.
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        entry
            .unpack(&target)
            .map_err(|e| format!("failed to unpack {}: {e}", relative.display()))?;
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("invalid SDK archive: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("corrupt SDK archive: {e}"))?;
        // `enclosed_name` returns None for anything that would escape the destination.
        let Some(path) = file.enclosed_name() else {
            return Err(format!("SDK archive contains an unsafe path: {}", file.name()));
        };
        let Some(relative) = strip_root(&path) else {
            continue;
        };
        let target = dest.join(&relative);

        if file.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| format!("failed to write {}: {e}", target.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|e| e.to_string())?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

// --- Vulkan loader ----------------------------------------------------------------------

/// Where the host's own Vulkan loader lives, if it has one.
///
/// Unlike glm/imgui/spdlog, the Vulkan *loader* is not an app-level library the SDK can
/// legitimately vendor: its entire job is to find the ICD that ships with the machine's GPU
/// driver, so it is part of the driver stack. Any machine that can run Vulkan already has one.
fn system_vulkan_loader() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let candidates: &[&str] = &[
        "/usr/lib/libvulkan.so.1",
        "/usr/lib64/libvulkan.so.1",
        "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
        "/lib/x86_64-linux-gnu/libvulkan.so.1",
        "/usr/lib/aarch64-linux-gnu/libvulkan.so.1",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &[r"C:\Windows\System32\vulkan-1.dll"];
    // macOS has no system loader — Vulkan arrives via MoltenVK, which the SDK is entitled to
    // ship. Leave whatever it bundles alone.
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[];

    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// Is this file the Vulkan loader (any soname flavour)?
fn is_vulkan_loader(file_name: &str) -> bool {
    let n = file_name.to_ascii_lowercase();
    n.starts_with("libvulkan.so")      // libvulkan.so, .so.1, .so.1.4.335
        || n.starts_with("libvulkan.1.dylib")
        || n == "vulkan-1.dll"
}

/// Delete a Vulkan loader bundled inside an SDK, so the runtime binds the host's instead.
///
/// Koral SDK 0.0.2 ships `lib/libvulkan.so.1` (1.4.335). It enumerates the GPU correctly but
/// leaves the `VK_KHR_surface` entry points unresolved, so the framework calls a null pointer
/// and segfaults in `kor::vk::Queue::Family::RequestPresentQueue` while building the swapchain.
/// The host's loader works. The SDK's `RUNPATH` is `$ORIGIN`, so simply removing the file is
/// enough to make the dynamic linker fall back to the system one.
///
/// Best-effort and deliberately conservative: if the host has no loader of its own, the
/// bundled one is all there is and it is left in place. Fixing the release to stop shipping a
/// loader is the real fix; this keeps already-published SDKs — and any future regression —
/// from being dead on arrival.
fn prefer_system_vulkan(sdk_root: &Path) {
    if system_vulkan_loader().is_none() {
        return;
    }
    for dir in ["lib", "bin"] {
        let Ok(entries) = std::fs::read_dir(sdk_root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            if is_vulkan_loader(&entry.file_name().to_string_lossy()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// --- Manifest synthesis -----------------------------------------------------------------

/// Write `framework.json` describing the unpacked SDK.
///
/// The published SDK does not carry one (the framework's CMake install does not emit it), so
/// the Hub derives it from the tree instead of hardcoding paths that a rename would break.
/// A release that *does* ship its own manifest is left untouched — it is the better authority.
fn write_manifest(
    root: &Path,
    version: &str,
    platform: &str,
    asset_name: &str,
) -> Result<(), String> {
    if root.join("framework.json").exists() {
        return Ok(());
    }

    // "gfx-sdk-0.0.1-linux-x64.tar.gz" -> "gfx". Survives the rename to "koral".
    let name = asset_name
        .split("-sdk-")
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("koral")
        .to_string();

    let manifest = FrameworkManifest {
        name,
        version: version.to_string(),
        platform: platform.to_string(),
        runtime: find_runtime(root)?,
        cmake_dir: find_cmake_dir(root)?,
        vcpkg_baseline: String::new(),
        };

    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(root.join("framework.json"), text)
        .map_err(|e| format!("failed to write framework.json: {e}"))
}

/// The executable under `bin/` that loads scenes. Prefers a name containing "runtime"
/// (`Gfx_Runtime`, and `Koral_Runtime` after the rename) and falls back to the only
/// executable present, so a rename of the binary itself does not need a Hub change.
fn find_runtime(root: &Path) -> Result<String, String> {
    let bin = root.join("bin");
    let mut candidates: Vec<String> = std::fs::read_dir(&bin)
        .map_err(|e| format!("SDK has no bin/ directory: {e}"))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // On Windows only .exe is runnable; on unix everything in bin/ is a candidate.
        .filter(|n| !cfg!(windows) || n.ends_with(".exe"))
        .collect();

    candidates.sort_by_key(|n| !n.to_ascii_lowercase().contains("runtime"));
    candidates
        .first()
        .map(|n| format!("bin/{n}"))
        .ok_or_else(|| format!("no runtime executable found in {}", bin.display()))
}

/// The directory under `lib/cmake/` holding the package config a consumer points
/// `CMAKE_PREFIX_PATH` at — `lib/cmake/GFX_RELOADED` now, `lib/cmake/Koral` later.
fn find_cmake_dir(root: &Path) -> Result<String, String> {
    let cmake = root.join("lib").join("cmake");
    std::fs::read_dir(&cmake)
        .map_err(|e| format!("SDK has no lib/cmake/ directory: {e}"))?
        .flatten()
        .map(|e| e.path())
        .find(|dir| {
            std::fs::read_dir(dir)
                .map(|mut entries| {
                    entries.any(|f| {
                        f.map(|f| f.file_name().to_string_lossy().ends_with("Config.cmake"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .and_then(|dir| dir.file_name().map(|n| format!("lib/cmake/{}", n.to_string_lossy())))
        .ok_or_else(|| format!("no *Config.cmake found under {}", cmake.display()))
}


