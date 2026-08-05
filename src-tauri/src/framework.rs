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
    /// What a project writes in its `frameworkVersion` to target this SDK: a release version
    /// (`0.0.10`) for a download, or a source pin (`source`, `source:<name>`) for a local build.
    pub version: String,
    /// What to show a human. The version for a release; the install prefix's folder name for a
    /// source build, which has no version to show — see [`local`].
    pub name: String,
    pub platform: String,
    pub path: String,
    /// Bytes on disk. Shown in the UI so it is obvious what uninstalling reclaims. Zero for a
    /// local build — the Hub did not put it there and removing the registration reclaims nothing.
    pub size_bytes: u64,
    /// Registered from a path rather than downloaded. Such an entry may be *forgotten* but never
    /// deleted, so the UI must offer "Remove" rather than "Uninstall".
    #[serde(default)]
    pub local: bool,
    /// The source tree this was built from, when it is known — what makes stepping into framework
    /// code work while debugging a project. Only ever set for a local build.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<String>,
    /// `CMAKE_BUILD_TYPE` of the build this was installed from, read fresh from its CMake cache.
    /// `None` when the build tree could not be found. Surfaced because a Release build carries no
    /// debug info, so a crash in it can never land on a line of framework source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_type: Option<String>,
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
            // Via `external_command` so it doesn't flash a console window on Windows.
            let out = crate::builder::external_command("gh")
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
        // Connect only: an SDK is a hundred megabytes and may legitimately take minutes, but a
        // host that never answers must not hold up a command the UI is waiting on — the release
        // list is now consulted for a new project's default version.
        .connect_timeout(std::time::Duration::from_secs(10))
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

/// What a project writes in `frameworkVersion` to target this machine's framework source build.
///
/// A build from source has no version, on purpose: it is a working tree that changes under you,
/// so a number stamped on it would be a lie the moment you rebuilt. It is pinned by *kind*
/// instead — "whatever source build this machine has registered" — and resolved fresh on every
/// build. `source:<name>` names one specifically, for a machine that has registered several.
pub const SOURCE_PIN: &str = "source";

/// The name a source pin selects: `Some("")` for a bare `source` (this machine's only source
/// build), `Some(name)` for `source:<name>`, `None` for anything that is not a source pin.
pub fn source_pin(version: &str) -> Option<&str> {
    let rest = version.strip_prefix(SOURCE_PIN)?;
    match rest.strip_prefix(':') {
        Some(name) => Some(name.trim()),
        None if rest.is_empty() => Some(""),
        // "sourceforge-1.0" is a version, not a pin.
        None => None,
    }
}

/// Is this what a project targeting a source build writes?
pub fn is_source_pin(version: &str) -> bool {
    source_pin(version).is_some()
}

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

/// Order a version so the newest sorts last: numerically, segment by segment, because a string
/// sort puts 0.10.0 *before* 0.9.0 and "newest installed" is the default a new project gets.
///
/// Each segment is a number plus whatever trails it, and a segment with nothing trailing wins:
/// 1.0.0 is newer than 1.0.0-rc2, which is newer than 1.0.0-rc1.
fn version_key(version: &str) -> Vec<(u64, u8, String)> {
    version
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            let rest = part[digits.len()..].to_string();
            let released = u8::from(rest.is_empty());
            (digits.parse().unwrap_or(0), released, rest)
        })
        .collect()
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
                name: version_name.clone(),
                version: version_name.clone(),
                platform: platform.file_name().to_string_lossy().into_owned(),
                size_bytes: dir_size(&dir),
                path: dir.to_string_lossy().into_owned(),
                local: false,
                source_dir: None,
                build_type: None,
            });
        }
    }

    out.sort_by(|a, b| version_key(&b.version).cmp(&version_key(&a.version)));

    // Locally-registered SDKs are installed too, as far as everything downstream is concerned —
    // the module scan, the picker, "is this version available". They cannot collide with a release
    // any more (their pin is namespaced), and they go first because a machine that has one is being
    // used to work on the framework itself.
    let host = host_platform();
    // Pin down any build directory a registration is missing first, so the build type below is
    // reported for an SDK registered before that was possible rather than left blank forever.
    local::fill_missing_build_dirs();
    let locals = local::list();
    let single = locals.len() == 1;
    let mut head: Vec<InstalledFramework> = locals
        .into_iter()
        .map(|entry| InstalledFramework {
            version: entry.pin(single),
            name: entry.name,
            platform: host.clone(),
            size_bytes: 0,
            path: entry.path,
            local: true,
            source_dir: (!entry.source_dir.is_empty()).then_some(entry.source_dir),
            build_type: local::build_type(&entry.build_dir),
        })
        .collect();

    head.append(&mut out);
    head
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
        unwrap_single_root(&staging)?;
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

/// Ensure `version` is available on this machine, downloading it if necessary, and return its SDK
/// root. Silent, with no progress reporting.
///
/// A source pin is already "installed" by definition — it is a directory the user built — so this
/// resolves it rather than trying to download a release by that name.
pub fn ensure_installed(version: &str) -> Result<PathBuf, String> {
    if let Some(local) = local::resolve_pin(version)? {
        return Ok(PathBuf::from(local.path));
    }
    let dir = install_dir(version, &host_platform());
    if dir.join("framework.json").exists() {
        return Ok(dir);
    }
    install(version, |_, _| {})
}

/// Locate the SDK a project's `frameworkVersion` names, and describe it: its root and manifest.
///
/// A **source pin** (`source`, `source:<name>`) resolves to a registered local build and never
/// touches the network. A bare version resolves to a downloaded release — but a local build
/// registered under that exact name still wins, which is what keeps projects pinned to the old
/// version-label scheme working.
///
/// The release path downloads on demand. A local SDK needs no `framework.json` written into the
/// user's install prefix: the manifest is synthesised from the tree each time instead, so a
/// rebuild is picked up with nothing to refresh.
pub fn resolve(version: &str) -> Result<(PathBuf, FrameworkManifest), String> {
    if let Some(local) = local::resolve_pin(version)? {
        let root = PathBuf::from(&local.path);
        if !root.is_dir() {
            return Err(format!(
                "the source build '{}' is registered at {}, which no longer exists — \
                 re-register it, or remove it in Frameworks",
                local.name, local.path
            ));
        }
        let manifest = describe_tree(&root, "koral", version, &host_platform())?;
        return Ok((root, manifest));
    }

    let root = ensure_installed(version)?;
    let manifest = read_manifest(&root)?;
    Ok((root, manifest))
}

/// The SDK root for `version` **without ever downloading one** — `None` when this machine does not
/// have it yet. What anything that merely inspects an SDK (the module scan, the settings panel)
/// must use: opening a panel is not consent to pull 40 MB over the network.
pub fn installed_root(version: &str) -> Option<PathBuf> {
    if let Ok(Some(local)) = local::resolve_pin(version) {
        let root = PathBuf::from(local.path);
        return root.is_dir().then_some(root);
    }
    let dir = install_dir(version, &host_platform());
    dir.join("framework.json").exists().then_some(dir)
}

/// Directories a debugger should search for the framework's own source, for a project targeting
/// `version`. Empty unless this is a source build whose tree the Hub knows about — a downloaded
/// SDK ships no source, so there is nothing a debugger could show for a frame inside it.
///
/// The source root itself is enough for gdb (it searches recursively from what `directory` is
/// given), but the engine's own layout dirs are listed too so a partial checkout still resolves.
pub fn debug_source_dirs(version: &str) -> Vec<PathBuf> {
    let Ok(Some(local)) = local::resolve_pin(version) else {
        return Vec::new();
    };
    if local.source_dir.is_empty() {
        return Vec::new();
    }
    let root = PathBuf::from(&local.source_dir);
    if !root.is_dir() {
        return Vec::new();
    }
    let mut dirs = vec![root.clone()];
    for sub in ["src", "include", "modules", "engine"] {
        let dir = root.join(sub);
        if dir.is_dir() {
            dirs.push(dir);
        }
    }
    dirs
}

/// Framework builds from source that the user has registered by path.
///
/// Deliberately an index of paths rather than a copy: a developer building the framework from
/// source re-runs `cmake --install` constantly, and the Hub must see the new build immediately
/// rather than a snapshot taken when they registered it. For the same reason these carry **no
/// version** — a working tree changes under you, so any number stamped on it is stale as soon as
/// it is written. Projects target them by kind instead; see [`SOURCE_PIN`].
pub mod local {
    use super::*;

    /// One registered source build.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LocalFramework {
        /// Display name, and what `source:<name>` selects. Derived from the install prefix's
        /// folder name rather than typed, since there is nothing meaningful for a user to name it.
        ///
        /// Reads a pre-source-build registration's `version` label as the name, so a machine that
        /// registered one under the old scheme keeps it (and keeps resolving projects pinned to
        /// that label — see [`resolve_pin`]).
        #[serde(alias = "version")]
        pub name: String,
        /// The install prefix: the directory `cmake --install --prefix` produced.
        pub path: String,
        /// The source tree it was built from, discovered from the build's CMake cache. Empty when
        /// it could not be found; the user can point at it themselves.
        #[serde(default)]
        pub source_dir: String,
        /// The CMake build directory the install came from, kept so the *current* build type can
        /// be re-read on every listing rather than frozen at registration.
        #[serde(default)]
        pub build_dir: String,
    }

    impl LocalFramework {
        /// What a project writes to target this build. Bare `source` when it is the only one
        /// registered — the common case, and the one worth keeping readable — and `source:<name>`
        /// once there is a choice to disambiguate.
        pub fn pin(&self, only_one: bool) -> String {
            if only_one {
                SOURCE_PIN.to_string()
            } else {
                format!("{SOURCE_PIN}:{}", self.name)
            }
        }
    }

    #[derive(Default, Serialize, Deserialize)]
    struct Cache {
        frameworks: Vec<LocalFramework>,
    }

    fn load_cache() -> Cache {
        std::fs::read_to_string(paths::local_frameworks_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_cache(cache: &Cache) -> Result<(), String> {
        let file = paths::local_frameworks_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
        std::fs::write(file, text).map_err(|e| e.to_string())
    }

    /// Every registered source build whose install prefix still exists.
    pub fn list() -> Vec<LocalFramework> {
        load_cache()
            .frameworks
            .into_iter()
            .filter(|f| Path::new(&f.path).is_dir())
            .collect()
    }

    /// The source build registered under this exact name, if any.
    pub fn find(name: &str) -> Option<LocalFramework> {
        list().into_iter().find(|f| f.name == name)
    }

    /// Which registered source build a project's `frameworkVersion` selects, if any.
    ///
    /// - `source` — this machine's source build. An error when several are registered, since
    ///   picking one arbitrarily would silently build against the wrong framework.
    /// - `source:<name>` — that one specifically.
    /// - anything else — a release version, *unless* a source build happens to carry that exact
    ///   name, which is how a project pinned under the old version-label scheme still resolves.
    ///
    /// `Ok(None)` means "not a source build, go and resolve a release".
    pub fn resolve_pin(version: &str) -> Result<Option<LocalFramework>, String> {
        let Some(name) = source_pin(version) else {
            return Ok(find(version));
        };
        if !name.is_empty() {
            return match find(name) {
                Some(framework) => Ok(Some(framework)),
                None => Err(format!(
                    "this project targets the source build '{name}', which is not registered on \
                     this machine — add it under Frameworks, or pick another framework"
                )),
            };
        }

        let mut all = list();
        match all.len() {
            0 => Err("this project targets a framework build from source, but none is \
                      registered on this machine — add one under Frameworks → Add source build"
                .into()),
            1 => Ok(Some(all.remove(0))),
            _ => Err(format!(
                "several source builds are registered ({}) — pin one in this project's settings \
                 so it is unambiguous which to build against",
                all.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// `CMAKE_BUILD_TYPE` of a build directory, read fresh so a rebuild in another configuration
    /// is reflected without re-registering. `None` when the directory is not a CMake build tree.
    ///
    /// Worth surfacing: a Release build of the framework carries no debug info, so no amount of
    /// source-path configuration will make a crash inside it land on a line of source.
    pub fn build_type(build_dir: &str) -> Option<String> {
        if build_dir.is_empty() {
            return None;
        }
        let text = std::fs::read_to_string(Path::new(build_dir).join("CMakeCache.txt")).ok()?;
        cache_value(&text, "CMAKE_BUILD_TYPE").map(str::to_string)
    }

    /// The value of a CMake cache entry (`NAME:TYPE=value`), ignoring the type.
    fn cache_value<'a>(cache: &'a str, key: &str) -> Option<&'a str> {
        cache.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            let (_, value) = rest.split_once('=')?;
            let value = value.trim();
            (!value.is_empty()).then_some(value)
        })
    }

    /// Directory names a CMake build tree conventionally has. Checked by name under each candidate
    /// so a build nested one level deeper than the prefix (`~/koral-sdk` installed from
    /// `~/dev/Koral/build`) is still found, without fanning out over every directory twice.
    const BUILD_DIR_NAMES: &[&str] = &[
        "build",
        "out",
        "cmake-build-debug",
        "cmake-build-release",
        "cmake-build-relwithdebinfo",
    ];

    /// Directories worth looking in for the build tree behind an install prefix: the prefix's
    /// ancestors, their immediate subdirectories, and the conventionally named build directories
    /// under those.
    fn build_candidates(prefix: &Path) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for ancestor in prefix.ancestors().skip(1).take(3) {
            candidates.push(ancestor.to_path_buf());
            let Ok(entries) = std::fs::read_dir(ancestor) else {
                continue;
            };
            // Bounded: a home directory can hold a lot, and this runs while the user waits.
            for entry in entries.flatten().take(200) {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    || entry.file_name().to_string_lossy().starts_with('.')
                {
                    continue;
                }
                let dir = entry.path();
                candidates.extend(BUILD_DIR_NAMES.iter().map(|name| dir.join(name)));
                candidates.push(dir);
            }
        }
        candidates
    }

    /// Does this directory hold the framework's own CMake project?
    ///
    /// Matched by the `project()` name rather than by folder name, because the folder is whatever
    /// the user cloned into — and tolerant of the pending GFX -> Koral rename, exactly like
    /// [`is_sdk_asset`], so a checkout of either era is recognised.
    fn is_framework_source(dir: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(dir.join("CMakeLists.txt")) else {
            return false;
        };
        text.lines()
            .filter_map(|line| line.trim_start().strip_prefix("project("))
            .any(|rest| {
                let name = rest.trim_start().to_ascii_lowercase();
                name.starts_with("koral") || name.starts_with("gfx")
            })
    }

    /// Find the source tree an install prefix was built from, and the build tree it came from when
    /// that can be pinned down.
    ///
    /// The answer decides which files a debugger opens for a frame inside the framework, and
    /// whether its build type can be reported at all, so it is established in descending order of
    /// confidence:
    ///
    /// 1. A build whose **install manifest** names files under this prefix. CMake writes
    ///    `install_manifest.txt` on every install, listing the files it actually wrote — so unlike
    ///    the cache it records the prefix `--install --prefix` was given. That makes the build that
    ///    produced this tree identifiable for the ordinary `cmake --install <build> --prefix
    ///    <src>/stage/sdk` layout, which is the whole reason step 2 is not enough on its own.
    ///    Newest install first: a prefix installed to twice holds what the later one wrote.
    /// 2. A `CMakeCache.txt` whose `CMAKE_INSTALL_PREFIX` is exactly this prefix — the build was
    ///    configured to install here, even if it has not yet done so and has no manifest.
    /// 3. A build tree whose source directory *contains* this prefix. An inference, not an
    ///    identification: it says which source tree, but not which of its build directories, so
    ///    the build type is left unreported rather than guessed at from whichever was found first.
    /// 4. Failing any cache at all — a build directory that has since been cleaned — an ancestor
    ///    of the prefix that is the framework's own CMake project.
    ///
    /// Returns `(source_dir, build_dir)`; both empty when nothing matched, and `build_dir` empty
    /// whenever the source was inferred rather than identified.
    pub fn discover_source(prefix: &Path) -> (String, String) {
        // Read each cache once. Capped: a tree of build directories is a lot of megabytes, and
        // nothing beyond the first handful is plausibly the one.
        let caches: Vec<(PathBuf, String)> = build_candidates(prefix)
            .into_iter()
            .filter_map(|dir| {
                let text = std::fs::read_to_string(dir.join("CMakeCache.txt")).ok()?;
                Some((dir, text))
            })
            .take(40)
            .collect();

        let source_of = |text: &str| {
            cache_value(text, "CMAKE_HOME_DIRECTORY")
                .filter(|s| Path::new(s).is_dir())
                .map(str::to_string)
        };

        // 1. The build whose install manifest says it wrote this tree, most recent install first.
        let mut installed_here: Vec<(&PathBuf, &String, std::time::SystemTime)> = caches
            .iter()
            .filter_map(|(dir, text)| {
                let manifest = dir.join("install_manifest.txt");
                let at = std::fs::metadata(&manifest).and_then(|m| m.modified()).ok()?;
                let listing = std::fs::read_to_string(&manifest).ok()?;
                // Compared as paths, not strings, so a neighbouring `…/stage/sdk-old` cannot
                // pass for `…/stage/sdk`.
                let wrote_here = listing
                    .lines()
                    .any(|line| Path::new(line.trim()).starts_with(prefix));
                wrote_here.then_some((dir, text, at))
            })
            .collect();
        installed_here.sort_by_key(|(_, _, at)| std::cmp::Reverse(*at));
        for (dir, text, _) in installed_here {
            if let Some(source) = source_of(text) {
                return (source, dir.to_string_lossy().into_owned());
            }
        }

        // 2. The build configured to install exactly here.
        for (dir, text) in &caches {
            let installs_here = cache_value(text, "CMAKE_INSTALL_PREFIX")
                .map(|p| Path::new(p) == prefix)
                .unwrap_or(false);
            if installs_here {
                if let Some(source) = source_of(text) {
                    return (source, dir.to_string_lossy().into_owned());
                }
            }
        }

        // 3. A build whose source tree contains this prefix.
        for (_, text) in &caches {
            if let Some(source) = source_of(text) {
                if prefix.starts_with(&source) {
                    return (source, String::new());
                }
            }
        }

        // 4. No usable cache: an ancestor that is the framework's source.
        for ancestor in prefix.ancestors().skip(1).take(4) {
            if is_framework_source(ancestor) {
                return (ancestor.to_string_lossy().into_owned(), String::new());
            }
        }
        (String::new(), String::new())
    }

    /// Register the install prefix at `path` as a source build.
    ///
    /// Validated before it is remembered — a directory that is not an SDK is rejected here, where
    /// the user can fix the path, rather than at the first build of a project that targets it.
    /// Re-registering the same prefix replaces the entry, so re-pointing at a moved source tree is
    /// one action. `source_dir` overrides what the build's CMake cache says; empty means "work it
    /// out", which is what the UI sends unless the user picked a folder by hand.
    pub fn add(path: &Path, source_dir: &str) -> Result<LocalFramework, String> {
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        // Canonicalised so the same tree reached by two different paths is one registration, and
        // so a relative path typed into the dialog is stored as something that still resolves
        // from wherever the Hub runs next.
        let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;

        // The real check: does this tree look like an installed SDK? describe_tree fails with a
        // specific reason ("SDK has no bin/ directory", "no *Config.cmake found under …"), which
        // is far more useful than a generic rejection.
        describe_tree(&path, "koral", SOURCE_PIN, &host_platform()).map_err(|e| {
            format!(
                "{} does not look like an installed Koral SDK — {e}. Point at the directory you \
                 passed to `cmake --install --prefix`.",
                path.display()
            )
        })?;

        let (discovered, build_dir) = discover_source(&path);
        let source_dir = match source_dir.trim() {
            "" => discovered,
            given => {
                if !Path::new(given).is_dir() {
                    return Err(format!("{given} is not a directory"));
                }
                given.to_string()
            }
        };

        // Named after the *source* tree when we know it, because that is the name a human has for
        // this framework ("GFX", "Koral"). An install prefix is almost always called something
        // generic — `sdk`, `install`, `stage/sdk` — which would make every registration on a
        // machine look alike, and `source:sdk` a meaningless thing to pin a project to.
        let folder_name = |dir: &Path| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
        };
        let name = folder_name(Path::new(&source_dir))
            .or_else(|| folder_name(&path))
            .unwrap_or_else(|| "source".to_string());

        let entry = LocalFramework {
            name,
            path: path.to_string_lossy().into_owned(),
            source_dir,
            build_dir,
        };

        let mut cache = load_cache();
        // Keyed by path: the prefix is the identity, and the name is only what we call it. Names
        // are deduplicated after, so two prefixes with the same folder name stay distinguishable.
        cache.frameworks.retain(|f| f.path != entry.path);
        cache.frameworks.push(entry.clone());
        dedupe_names(&mut cache.frameworks);
        save_cache(&cache)?;
        // The stored entry may have been renamed to keep names unique; hand back what was saved.
        Ok(cache
            .frameworks
            .iter()
            .find(|f| f.path == entry.path)
            .cloned()
            .unwrap_or(entry))
    }

    /// Make every name unique, since `source:<name>` has to select exactly one build. A clash
    /// takes a numeric suffix (`koral-install-2`), which is stable as long as the list is.
    fn dedupe_names(frameworks: &mut [LocalFramework]) {
        let mut seen: Vec<String> = Vec::new();
        for framework in frameworks.iter_mut() {
            let base = framework.name.clone();
            let mut candidate = base.clone();
            let mut n = 1;
            while seen.contains(&candidate) {
                n += 1;
                candidate = format!("{base}-{n}");
            }
            seen.push(candidate.clone());
            framework.name = candidate;
        }
    }

    /// Forget a source build, by name. Never touches the directory itself — it is the user's build
    /// output, not something the Hub installed and may delete.
    pub fn remove(name: &str) -> Result<(), String> {
        let mut cache = load_cache();
        cache.frameworks.retain(|f| f.name != name);
        save_cache(&cache)
    }

    /// Point an existing registration at a source tree the user picked by hand — the escape hatch
    /// for a build whose CMake cache is gone (a fresh clone of the prefix, a pruned build dir).
    pub fn set_source_dir(name: &str, source_dir: &str) -> Result<(), String> {
        let source_dir = source_dir.trim();
        if !source_dir.is_empty() && !Path::new(source_dir).is_dir() {
            return Err(format!("{source_dir} is not a directory"));
        }
        let mut cache = load_cache();
        let entry = cache
            .frameworks
            .iter_mut()
            .find(|f| f.name == name)
            .ok_or_else(|| format!("no source build named '{name}' is registered"))?;
        entry.source_dir = source_dir.to_string();
        save_cache(&cache)
    }

    /// Work out the build directory for any registration that has none, and remember it.
    ///
    /// An empty `build_dir` means no build type to report, and so a Frameworks tab that cannot say
    /// whether a source build carries debug info at all. Registrations made before the install
    /// manifest was consulted all carry one — [`discover_source`] had nothing that could identify
    /// the build behind a `--install --prefix` tree — so discovery is re-run for them here rather
    /// than making the user re-register a framework that has not changed.
    ///
    /// Only ever fills a gap: a `source_dir` the user set by hand is left exactly as it is.
    ///
    /// Once found the answer is saved, so this costs nothing on later listings. An entry that
    /// stays unidentifiable — its build tree deleted — is searched again each time, which is the
    /// same bounded scan the Add dialog already runs, and only while the Frameworks tab is open.
    pub fn fill_missing_build_dirs() {
        let mut cache = load_cache();
        let mut found_any = false;

        for entry in &mut cache.frameworks {
            if !entry.build_dir.is_empty() || !Path::new(&entry.path).is_dir() {
                continue;
            }
            let (source, build) = discover_source(Path::new(&entry.path));
            if build.is_empty() {
                continue;
            }
            entry.build_dir = build;
            if entry.source_dir.is_empty() {
                entry.source_dir = source;
            }
            found_any = true;
        }

        if found_any {
            let _ = save_cache(&cache);
        }
    }
}

// --- Unpacking --------------------------------------------------------------------------

/// Reduce an archive entry's path to a plain relative path, or `None` if it is not one.
///
/// Rejecting `..`/absolute components is what keeps an entry from escaping the install
/// directory — the callers join this onto `dest` and write there directly, so this is the only
/// containment check there is.
fn safe_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let mut rest = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(c) => rest.push(c),
            Component::CurDir => {}
            // `..`, `/`, or a Windows prefix — nothing legitimate in an SDK archive.
            _ => return None,
        }
    }
    (!rest.as_os_str().is_empty()).then_some(rest)
}

/// Lift the contents of a lone top-level directory up into `dest`, so the SDK root *is* the
/// install dir and paths in `framework.json` stay relative to something stable.
///
/// The archives disagree about whether they wrap: the tarballs nest everything under a
/// directory named after the archive (`koral-sdk-0.0.5-linux-x64/bin/...`), while the Windows
/// zip stores `bin/...` at the top. Unpacking verbatim and unwrapping afterwards handles both
/// without having to guess from entry paths mid-extraction — an already-flat tree has more
/// than one top-level entry (`bin`, `include`, `lib`), so it is left alone.
fn unwrap_single_root(dest: &Path) -> Result<(), String> {
    let mut entries = std::fs::read_dir(dest)
        .map_err(|e| format!("failed to read {}: {e}", dest.display()))?
        .flatten();
    let (Some(only), None) = (entries.next(), entries.next()) else {
        return Ok(()); // empty, or already flat
    };
    if !only.path().is_dir() {
        return Ok(());
    }

    let wrapper = only.path();
    for child in std::fs::read_dir(&wrapper)
        .map_err(|e| format!("failed to read {}: {e}", wrapper.display()))?
        .flatten()
    {
        let to = dest.join(child.file_name());
        std::fs::rename(child.path(), &to)
            .map_err(|e| format!("failed to move {} out of the archive wrapper: {e}", to.display()))?;
    }
    std::fs::remove_dir(&wrapper).map_err(|e| e.to_string())
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
        let Some(relative) = safe_path(&path) else {
            continue; // an entry that refuses to sit under `dest`
        };
        let target = dest.join(&relative);

        // NOT `unpack_in`: it re-resolves the entry against its own archived path rather than
        // the one we vetted. `unpack` writes exactly where it is told — `safe_path` is what
        // keeps that in bounds.
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
        let Some(relative) = safe_path(&path) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("koral-framework-test-{n}"))
    }

    fn zip_of(entries: &[&str]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        for entry in entries {
            if let Some(dir) = entry.strip_suffix('/') {
                w.add_directory(dir, opts).unwrap();
            } else {
                w.start_file(*entry, opts).unwrap();
                std::io::Write::write_all(&mut w, b"x").unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    /// A string sort reads 0.10.0 as older than 0.9.0, which would hand every new project a
    /// two-releases-stale default the day the framework reaches its tenth minor.
    #[test]
    fn versions_order_by_number_not_by_text() {
        let mut versions = ["0.9.0", "0.10.0", "0.0.3", "1.0.0-rc1", "1.0.0", "0.10.1"];
        versions.sort_by_key(|v| version_key(v));
        assert_eq!(
            versions,
            ["0.0.3", "0.9.0", "0.10.0", "0.10.1", "1.0.0-rc1", "1.0.0"]
        );
    }

    /// The tarballs wrap everything in a directory named after the archive; the Windows zip
    /// does not. Both must land with `bin/` directly under the install root — stripping a
    /// component unconditionally left Windows installs with their files loose at the root and
    /// no `bin/` at all.
    #[test]
    fn both_wrapped_and_flat_archives_unpack_to_the_same_root() {
        let base = scratch();

        for (case, entries) in [
            ("wrapped", &["koral-sdk-0.0.5-windows-x64/bin/", "koral-sdk-0.0.5-windows-x64/bin/Koral_Runtime.exe"][..]),
            ("flat", &["bin/", "bin/Koral_Runtime.exe", "include/", "include/api.h"][..]),
        ] {
            let dest = base.join(case);
            std::fs::create_dir_all(&dest).unwrap();
            extract_zip(&zip_of(entries), &dest).unwrap();
            unwrap_single_root(&dest).unwrap();

            assert!(
                dest.join("bin/Koral_Runtime.exe").is_file(),
                "{case}: runtime should sit at <root>/bin/"
            );
            assert_eq!(find_runtime(&dest).unwrap(), "bin/Koral_Runtime.exe", "{case}");
        }

        std::fs::remove_dir_all(&base).ok();
    }

    /// Build a directory shaped like a `cmake --install` prefix of the SDK.
    fn fake_sdk(root: &Path) {
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("lib/cmake/Koral")).unwrap();
        let runtime = if cfg!(windows) { "Koral_Runtime.exe" } else { "Koral_Runtime" };
        std::fs::write(root.join("bin").join(runtime), "x").unwrap();
        std::fs::write(root.join("lib/cmake/Koral/KoralConfig.cmake"), "x").unwrap();
    }

    /// A local SDK is described by inspecting the tree — no framework.json required, and none
    /// written. The engine's own `cmake --install` produces no manifest, so requiring one would
    /// make every source build unusable; and writing one into the user's prefix would leave a
    /// file that goes stale the moment they re-point the registration.
    #[test]
    fn a_source_install_is_described_without_a_manifest() {
        let root = scratch();
        fake_sdk(&root);

        let manifest = describe_tree(&root, "koral", "0.0.10", "linux-x64").unwrap();
        assert_eq!(manifest.version, "0.0.10");
        assert_eq!(manifest.cmake_dir, "lib/cmake/Koral");
        assert!(manifest.runtime.starts_with("bin/Koral_Runtime"));
        assert!(
            !root.join("framework.json").exists(),
            "describing a tree must not write into it"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A directory that is not an SDK is rejected with a reason naming what is missing, rather
    /// than being accepted and failing at the first build of a project that targets it.
    #[test]
    fn a_directory_that_is_not_an_sdk_is_rejected_with_a_reason() {
        let root = scratch();
        std::fs::create_dir_all(&root).unwrap();

        // Nothing at all: no bin/.
        let err = describe_tree(&root, "koral", "0.0.10", "linux-x64").unwrap_err();
        assert!(err.contains("bin"), "{err}");

        // A runtime but no CMake package config — buildable-looking, but a consumer could not
        // find_package(Koral) against it.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let runtime = if cfg!(windows) { "Koral_Runtime.exe" } else { "Koral_Runtime" };
        std::fs::write(root.join("bin").join(runtime), "x").unwrap();
        let err = describe_tree(&root, "koral", "0.0.10", "linux-x64").unwrap_err();
        assert!(err.contains("cmake"), "{err}");

        std::fs::remove_dir_all(&root).ok();
    }

    /// A source pin is what a project writes instead of a version. It has to be told apart from a
    /// version by *shape*, since both land in the same `frameworkVersion` field — and a release
    /// that merely starts with the letters "source" must stay a release.
    #[test]
    fn source_pins_are_told_apart_from_versions() {
        assert_eq!(source_pin("source"), Some(""));
        assert_eq!(source_pin("source:koral-install"), Some("koral-install"));
        assert_eq!(source_pin("source: koral "), Some("koral"));

        for version in ["0.0.10", "1.2.3-rc1", "sourceforge-1.0", "", "resource"] {
            assert_eq!(source_pin(version), None, "{version} is a version, not a pin");
            assert!(!is_source_pin(version));
        }
    }

    /// The source tree is identified from the build's own CMake cache rather than guessed, because
    /// the answer decides which files a debugger opens for a frame inside the framework. The match
    /// is on `CMAKE_INSTALL_PREFIX`: a *different* build tree sitting next door must not be
    /// mistaken for the one this prefix came from.
    #[test]
    fn the_source_tree_is_found_through_the_build_that_installed_it() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("koral-discover-{}", std::process::id()));
        let source = base.join("Koral");
        let build = base.join("Koral/build");
        let prefix = base.join("koral-sdk");
        let other = base.join("SomethingElse/build");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let cache = |install: &Path, home: &Path, build_type: &str| {
            format!(
                "CMAKE_INSTALL_PREFIX:PATH={}\nCMAKE_HOME_DIRECTORY:INTERNAL={}\n\
                 CMAKE_BUILD_TYPE:STRING={build_type}\n",
                install.display(),
                home.display()
            )
        };
        std::fs::write(build.join("CMakeCache.txt"), cache(&prefix, &source, "Debug")).unwrap();
        // A build tree that installs somewhere else entirely — it must not be picked up.
        std::fs::write(
            other.join("CMakeCache.txt"),
            cache(&base.join("elsewhere"), &base.join("SomethingElse"), "Release"),
        )
        .unwrap();

        let (found_source, found_build) = local::discover_source(&prefix);
        assert_eq!(Path::new(&found_source), source);
        assert_eq!(Path::new(&found_build), build);
        assert_eq!(local::build_type(&found_build).as_deref(), Some("Debug"));

        // A prefix with no build tree to be found reports nothing rather than guessing.
        std::fs::remove_file(build.join("CMakeCache.txt")).unwrap();
        assert_eq!(local::discover_source(&prefix), (String::new(), String::new()));
        assert_eq!(local::build_type(""), None);

        std::fs::remove_dir_all(&base).ok();
    }

    /// The layout `cmake --install <build> --prefix <src>/stage/sdk` produces — which is how the
    /// SDK is normally staged, and what the README told people to do.
    ///
    /// `--install --prefix` overrides the prefix for that one command and never writes it to the
    /// cache, so no build tree records installing here. Matching only on `CMAKE_INSTALL_PREFIX`
    /// therefore finds nothing at all, and the source build silently becomes undebuggable — the
    /// exact thing the feature exists to prevent. The prefix sitting *inside* the source tree is
    /// what identifies it instead.
    ///
    /// This is the case with no install manifest to go on — a build tree cleaned since it staged
    /// this prefix. The source tree is still found; which build produced it is not, so the build
    /// type stays unreported. With a manifest, both are known (see the test below).
    #[test]
    fn a_prefix_staged_inside_the_source_tree_is_recognised() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("koral-staged-{}", std::process::id()));
        let source = base.join("GFX");
        let build = source.join("cmake-build-debug");
        let prefix = source.join("stage/sdk");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();

        // Configured to install to /usr/local, then staged elsewhere with `--install --prefix`.
        std::fs::write(
            build.join("CMakeCache.txt"),
            format!(
                "CMAKE_INSTALL_PREFIX:PATH=/usr/local\n\
                 CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_BUILD_TYPE:STRING=Debug\n",
                source.display()
            ),
        )
        .unwrap();

        let (found_source, found_build) = local::discover_source(&prefix);
        assert_eq!(Path::new(&found_source), source);
        assert_eq!(
            found_build, "",
            "which build installed here is not recorded anywhere, so it must not be claimed"
        );

        // Even with the build tree gone, the source tree is still identifiable by its project().
        std::fs::remove_dir_all(&build).unwrap();
        std::fs::write(
            source.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.28)\nproject(Koral VERSION 0.0.9 LANGUAGES C CXX)\n",
        )
        .unwrap();
        assert_eq!(Path::new(&local::discover_source(&prefix).0), source);

        // A directory that is some other CMake project is not the framework.
        std::fs::write(source.join("CMakeLists.txt"), "project(SomethingElse)\n").unwrap();
        assert_eq!(local::discover_source(&prefix), (String::new(), String::new()));

        std::fs::remove_dir_all(&base).ok();
    }

    /// The same staged layout, but with the `install_manifest.txt` CMake writes on every install.
    ///
    /// It lists the files the install actually wrote, so it records the `--prefix` the cache never
    /// sees — which is what makes the build behind a staged prefix identifiable, and with it the
    /// build type. Without this the Frameworks tab can say nothing at all about a source build
    /// staged the ordinary way: not that it is debuggable, and not that a Release one is not.
    #[test]
    fn the_install_manifest_identifies_the_build_that_staged_the_prefix() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("koral-manifest-{}", std::process::id()));
        let source = base.join("GFX");
        let debug = source.join("cmake-build-debug");
        let release = source.join("cmake-build-release");
        let prefix = source.join("stage/sdk");
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::create_dir_all(&release).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();

        // Both configured to install to /usr/local and then staged with `--install --prefix`, so
        // neither records this prefix in its cache. Only one of them staged it *here*.
        let cache = |build_type: &str| {
            format!(
                "CMAKE_INSTALL_PREFIX:PATH=/usr/local\n\
                 CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_BUILD_TYPE:STRING={build_type}\n",
                source.display()
            )
        };
        std::fs::write(debug.join("CMakeCache.txt"), cache("Debug")).unwrap();
        std::fs::write(release.join("CMakeCache.txt"), cache("Release")).unwrap();

        let manifest = |dir: &Path, to: &Path| {
            std::fs::write(
                dir.join("install_manifest.txt"),
                format!(
                    "{}\n{}\n",
                    to.join("lib/libKoral.so").display(),
                    to.join("bin/Koral_Runtime").display()
                ),
            )
            .unwrap();
        };
        manifest(&debug, &prefix);
        // The release build staged somewhere else entirely, and must not be mistaken for this one.
        manifest(&release, &base.join("elsewhere"));

        let (found_source, found_build) = local::discover_source(&prefix);
        assert_eq!(Path::new(&found_source), source);
        assert_eq!(Path::new(&found_build), debug);
        assert_eq!(
            local::build_type(&found_build).as_deref(),
            Some("Debug"),
            "identifying the build is what makes its build type reportable"
        );

        // Installed to twice: the tree on disk is whatever the *later* install wrote, so that is
        // the build — and the build type — to report.
        manifest(&release, &prefix);
        let touch = |dir: &Path, secs: u64| {
            let file = std::fs::File::options()
                .write(true)
                .open(dir.join("install_manifest.txt"))
                .unwrap();
            let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            file.set_times(std::fs::FileTimes::new().set_modified(at)).unwrap();
        };
        touch(&debug, 1_000_000);
        touch(&release, 2_000_000);
        assert_eq!(Path::new(&local::discover_source(&prefix).1), release);
        touch(&debug, 3_000_000);
        assert_eq!(Path::new(&local::discover_source(&prefix).1), debug);

        // A neighbouring prefix with a similar name is a different tree, not this one.
        let sibling = source.join("stage/sdk-old");
        std::fs::create_dir_all(&sibling).unwrap();
        assert_eq!(
            local::discover_source(&sibling).1,
            "",
            "no manifest names this prefix, so no build may be claimed for it"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// Unwrapping keys off "exactly one top-level entry", so an archive that legitimately has
    /// a single top-level *file* beside nothing must not be mistaken for a wrapper.
    #[test]
    fn unwrap_leaves_a_lone_top_level_file_alone() {
        let dest = scratch();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("framework.json"), "{}").unwrap();

        unwrap_single_root(&dest).unwrap();
        assert!(dest.join("framework.json").is_file());

        std::fs::remove_dir_all(&dest).ok();
    }
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
/// Describe an unpacked SDK tree by inspecting it, without writing anything.
///
/// Shared by the release path (which then persists the result) and by locally-registered SDKs,
/// where nothing is written at all — a `cmake --install` prefix belongs to the user, and a
/// framework.json the Hub dropped into it would linger and go stale the moment they re-point the
/// registration at a different version.
///
/// Doubles as the validity check for a directory the user picked: a tree with no runtime, or no
/// `*Config.cmake`, is not an SDK, and this is what says so in those words.
pub fn describe_tree(
    root: &Path,
    name: &str,
    version: &str,
    platform: &str,
) -> Result<FrameworkManifest, String> {
    Ok(FrameworkManifest {
        name: name.to_string(),
        version: version.to_string(),
        platform: platform.to_string(),
        runtime: find_runtime(root)?,
        cmake_dir: find_cmake_dir(root)?,
        vcpkg_baseline: String::new(),
    })
}

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
        .unwrap_or("koral");

    let manifest = describe_tree(root, name, version, platform)?;

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


