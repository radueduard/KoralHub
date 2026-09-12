//! The build tools a project needs before the Hub can compile anything: CMake, Ninja, a C++
//! compiler, and vcpkg.
//!
//! The Hub already installs the *framework* for you; this is the rest of the story. Until now a
//! machine missing any of these failed deep inside CMake — "No CMAKE_CXX_COMPILER could be found" —
//! which names nothing the user can act on. Here each dependency is checked by name, and the ones
//! the Hub can supply, it supplies.
//!
//! The split is not arbitrary, and it is the whole design:
//!
//!  - **CMake and Ninja** are portable archives on GitHub. They unpack into the Hub's own data
//!    directory, need no installer, no elevation and no `PATH` change, and are found again by
//!    absolute path. Exactly how a framework release is installed, for exactly the same reasons.
//!  - **vcpkg** already installs itself (see [`crate::vcpkg`]); this only reports on it.
//!  - **A C++ compiler cannot be installed this way on any platform**, and pretending otherwise
//!    would be the wrong kind of helpful. On Windows it means accepting a Microsoft licence and
//!    elevating; on Linux it means the system package manager and a root password; on macOS it is
//!    Apple's own installer. So the Hub detects, explains, and hands off to the official installer —
//!    it never accepts a licence or asks for a password on the user's behalf.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{framework, ide, paths};

/// Which dependency a [`Tool`] describes. The UI keys its rows and its install calls off this, so
/// the strings are part of the command surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolId {
    Cmake,
    Ninja,
    Compiler,
    Vcpkg,
}

/// Where a tool came from, which is what tells the user whether the Hub is responsible for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Not on this machine.
    Missing,
    /// Found on `PATH`, or in a place the system put it.
    System,
    /// Bundled inside something else — the ninja that ships with Visual Studio or CLion.
    Bundled,
    /// Downloaded by the Hub into its own data directory.
    Hub,
}

/// One dependency, as the wizard shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub id: ToolId,
    /// Display name ("CMake").
    pub name: String,
    /// One line on why a build needs it.
    pub purpose: String,
    pub source: Source,
    /// Where it was found, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Version string when cheaply available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// True when [`install`] can obtain this one without help.
    pub installable: bool,
    /// What the user has to do themselves, when the Hub cannot do it for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// A one-click hand-off to the official installer, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

impl Tool {
    fn found(&self) -> bool {
        self.source != Source::Missing
    }
}

/// Root of the tools the Hub installs itself: `<data>/tools/`.
fn tools_dir() -> PathBuf {
    paths::data_dir().join("tools")
}

// --- Detection ---------------------------------------------------------------------------

/// The CMake to build with: the Hub's own copy first, then whatever the system has.
///
/// Hub-first so that a copy the Hub installed is the one it drives, rather than an older system
/// CMake that happens to be on `PATH` and might not understand the presets being generated.
pub fn cmake_program() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "cmake.exe" } else { "cmake" };
    let hub = tools_dir().join("cmake").join("bin").join(exe);
    if hub.is_file() {
        return Some(hub);
    }
    ide::which("cmake").map(PathBuf::from)
}

/// The Ninja to build with: the Hub's own copy, the system's, then one bundled in an IDE.
///
/// The bundled fallback is last but far from unimportant — on Windows it is usually the only ninja
/// present, inside Visual Studio or CLion. See [`bundled_ninja_dirs`].
pub fn ninja_program() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ninja.exe" } else { "ninja" };
    let hub = tools_dir().join("ninja").join(exe);
    if hub.is_file() {
        return Some(hub);
    }
    if let Some(found) = ide::which("ninja") {
        return Some(PathBuf::from(found));
    }
    bundled_ninja_dirs()
        .into_iter()
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// Directories that ship a ninja alongside something else, newest-looking first.
///
/// Scanned rather than hard-coded by version, so a Visual Studio or CLion upgrade does not need a
/// Hub change — the version and edition are directory names under a stable root.
fn bundled_ninja_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if cfg!(windows) {
        // Visual Studio / Build Tools ship one at
        // <root>/<version>/<edition>/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja.
        // Both Program Files roots, since the installer has used each over the years.
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            let Some(root) = std::env::var_os(var) else {
                continue;
            };
            let root = PathBuf::from(root).join("Microsoft Visual Studio");
            for version in newest_first(&root) {
                for edition in newest_first(&version) {
                    dirs.push(edition.join("Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja"));
                }
            }
        }

        // JetBrains IDEs (CLion, and RustRover with the C/C++ plugin) bundle one per platform.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            for ide in newest_first(&PathBuf::from(local).join("Programs")) {
                dirs.push(ide.join("bin/ninja/win/x64"));
            }
        }
    } else {
        // A GUI-launched app can have a PATH that misses these even when a terminal finds them —
        // the same gap `repair_path` exists to close.
        for fixed in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin", "/opt/local/bin"] {
            dirs.push(PathBuf::from(fixed));
        }
    }

    dirs
}

/// Subdirectories of `dir`, newest-looking first (reverse name order, so "18" beats "17").
pub(crate) fn newest_first(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    found.sort();
    found.reverse();
    found
}

/// Is this path inside the Hub's own tools directory?
fn is_hub_installed(path: &Path) -> bool {
    path.starts_with(tools_dir())
}

/// Ask a tool for its version, cheaply and without ever failing the caller.
fn version_of(program: &Path, args: &[&str]) -> Option<String> {
    let output = crate::builder::external_command(program).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// Every dependency and its current state, for the wizard.
pub fn status() -> Vec<Tool> {
    vec![cmake_status(), ninja_status(), compiler_status(), vcpkg_status()]
}

fn cmake_status() -> Tool {
    let found = cmake_program();
    Tool {
        id: ToolId::Cmake,
        name: "CMake".into(),
        purpose: "Configures and drives every build.".into(),
        source: match &found {
            None => Source::Missing,
            Some(p) if is_hub_installed(p) => Source::Hub,
            Some(_) => Source::System,
        },
        version: found.as_deref().and_then(|p| version_of(p, &["--version"])),
        path: found.map(|p| p.to_string_lossy().into_owned()),
        installable: true,
        guidance: None,
        handoff: None,
    }
}

fn ninja_status() -> Tool {
    let found = ninja_program();
    Tool {
        id: ToolId::Ninja,
        name: "Ninja".into(),
        purpose: "The build tool CMake generates for — the same on every platform.".into(),
        source: match &found {
            None => Source::Missing,
            Some(p) if is_hub_installed(p) => Source::Hub,
            // On PATH is a deliberate install; inside Visual Studio or CLion is incidental, and
            // worth distinguishing so the user knows why a tool they never installed is there.
            Some(p) if ide::which("ninja").is_some_and(|w| Path::new(&w) == p.as_path()) => {
                Source::System
            }
            Some(_) => Source::Bundled,
        },
        version: found.as_deref().and_then(|p| version_of(p, &["--version"])),
        path: found.map(|p| p.to_string_lossy().into_owned()),
        installable: true,
        guidance: None,
        handoff: None,
    }
}

fn vcpkg_status() -> Tool {
    let root = crate::vcpkg::root();
    Tool {
        id: ToolId::Vcpkg,
        name: "vcpkg".into(),
        purpose: "Supplies the libraries a project declares. Only needed if it declares any.".into(),
        source: if root.is_some() { Source::Hub } else { Source::Missing },
        version: None,
        path: root.map(|p| p.to_string_lossy().into_owned()),
        // The Hub fetches it on its own in the background; the wizard only reports.
        installable: false,
        guidance: (crate::vcpkg::root().is_none())
            .then(|| "Fetched automatically in the background the first time a project needs it.".into()),
        handoff: None,
    }
}

/// The compiler: detected, explained, never installed.
fn compiler_status() -> Tool {
    let (source, path, version) = detect_compiler();
    let (guidance, handoff) = if source == Source::Missing {
        compiler_help()
    } else {
        (None, None)
    };

    Tool {
        id: ToolId::Compiler,
        name: compiler_name().into(),
        purpose: "Compiles your C++. The one dependency the Hub cannot install for you.".into(),
        source,
        path,
        version,
        installable: false,
        guidance,
        handoff,
    }
}

fn compiler_name() -> &'static str {
    if cfg!(windows) {
        "MSVC (Visual Studio Build Tools)"
    } else if cfg!(target_os = "macos") {
        "Xcode command line tools"
    } else {
        "GCC"
    }
}

/// Find the platform's C++ compiler, however it is normally reached.
fn detect_compiler() -> (Source, Option<String>, Option<String>) {
    #[cfg(windows)]
    {
        // Already on PATH (a developer prompt), or reachable once the Hub applies the MSVC
        // environment it works out for itself.
        if let Some(found) = ide::which("cl") {
            return (Source::System, Some(found), None);
        }
        // Not on PATH, but reachable once the Hub applies the MSVC environment it works out for
        // itself — which is exactly how every build the Hub runs will see it.
        let env = crate::msvc::environment();
        if !env.is_empty() {
            let cl = env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                .and_then(|(_, value)| {
                    std::env::split_paths(value)
                        .find(|dir| dir.join("cl.exe").is_file())
                        .map(|dir| dir.join("cl.exe").to_string_lossy().into_owned())
                });
            // Never report "found" with nothing to show for it. If the exact cl.exe could not be
            // picked out of that PATH, the environment still proves a toolchain is there, so say so
            // in words rather than leaving a row that reads as a blank — the state that makes a
            // detected compiler look undetected.
            let path = cl.unwrap_or_else(|| "found via the Visual Studio build environment".into());
            return (Source::System, Some(path), None);
        }
        (Source::Missing, None, None)
    }

    #[cfg(not(windows))]
    {
        for candidate in ["c++", "g++", "clang++"] {
            if let Some(found) = ide::which(candidate) {
                let version = version_of(Path::new(&found), &["--version"]);
                return (Source::System, Some(found), version);
            }
        }
        (Source::Missing, None, None)
    }
}

/// What to tell the user when there is no compiler, and what the Hub can open for them.
fn compiler_help() -> (Option<String>, Option<String>) {
    if cfg!(windows) {
        (
            Some(
                "Install the Visual Studio Build Tools and tick \"Desktop development with C++\". \
                 The Hub can start Microsoft's installer for you with that workload already \
                 selected — it needs administrator approval and is a large download, and you accept \
                 Microsoft's licence in their installer, not here."
                    .into(),
            ),
            Some("Open Microsoft's installer".into()),
        )
    } else if cfg!(target_os = "macos") {
        (
            Some(
                "Apple ships the compiler with the Xcode command line tools. The Hub can trigger \
                 Apple's installer, which then asks you to confirm."
                    .into(),
            ),
            Some("Install the command line tools".into()),
        )
    } else {
        (
            Some(format!(
                "Install it with your package manager, which needs a root password the Hub cannot \
                 (and should not) ask for:\n\n    {}\n\nInstalling the Hub from the .deb or .rpm \
                 pulls this in automatically.",
                linux_install_command()
            )),
            None,
        )
    }
}

/// The package-manager line for this distribution, picked by which manager exists.
fn linux_install_command() -> &'static str {
    for (manager, command) in [
        ("apt", "sudo apt install build-essential"),
        ("dnf", "sudo dnf install gcc-c++ make"),
        ("pacman", "sudo pacman -S base-devel"),
        ("zypper", "sudo zypper install gcc-c++ make"),
    ] {
        if ide::which(manager).is_some() {
            return command;
        }
    }
    "sudo apt install build-essential"
}

// --- Installing --------------------------------------------------------------------------

/// A GitHub release asset to fetch, resolved for this host.
struct Download {
    /// `owner/repo` on GitHub.
    repo: &'static str,
    /// Substrings the asset name must contain to be the one for this platform.
    matches: Vec<&'static str>,
    /// Where it unpacks to under `tools/`.
    dir: &'static str,
    /// Whether the archive wraps everything in a single top-level folder to strip.
    strip_root: bool,
}

fn cmake_download() -> Option<Download> {
    let matches = if cfg!(windows) {
        vec!["windows-x86_64", ".zip"]
    } else if cfg!(target_os = "macos") {
        vec!["macos-universal", ".tar.gz"]
    } else if cfg!(target_arch = "aarch64") {
        vec!["linux-aarch64", ".tar.gz"]
    } else {
        vec!["linux-x86_64", ".tar.gz"]
    };
    Some(Download { repo: "Kitware/CMake", matches, dir: "cmake", strip_root: true })
}

fn ninja_download() -> Option<Download> {
    let matches = if cfg!(windows) {
        vec!["ninja-win.zip"]
    } else if cfg!(target_os = "macos") {
        vec!["ninja-mac.zip"]
    } else if cfg!(target_arch = "aarch64") {
        vec!["ninja-linux-aarch64.zip"]
    } else {
        vec!["ninja-linux.zip"]
    };
    // The ninja archive is a bare binary, with no folder around it.
    Some(Download { repo: "ninja-build/ninja", matches, dir: "ninja", strip_root: false })
}


/// Download and unpack one of the tools the Hub can supply.
///
/// Installs into a staging directory and renames on success, so an interrupted download can never
/// leave a half-unpacked toolchain that later looks installed — the same contract as an SDK
/// install.
pub fn install(id: ToolId, mut progress: impl FnMut(u64, u64)) -> Result<String, String> {
    let download = match id {
        ToolId::Cmake => cmake_download(),
        ToolId::Ninja => ninja_download(),
        ToolId::Compiler | ToolId::Vcpkg => None,
    }
    .ok_or_else(|| format!("the Hub cannot install {id:?} on this platform"))?;

    let asset = latest_asset(&download)?;

    let mut response = framework::http()?
        .get(&asset.url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("failed to download {}: {e}", asset.name))?;

    let total = response.content_length().unwrap_or(asset.size);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        progress(bytes.len() as u64, total);
    }

    let dest = tools_dir().join(download.dir);
    let staging = dest.with_extension("downloading");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("failed to create {}: {e}", staging.display()))?;

    let unpacked = (|| -> Result<(), String> {
        if asset.name.ends_with(".zip") {
            framework::extract_zip(&bytes, &staging)?;
        } else {
            framework::extract_tar_gz(&bytes, &staging)?;
        }
        if download.strip_root {
            framework::unwrap_single_root(&staging)?;
        }
        Ok(())
    })();
    if let Err(e) = unpacked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // A downloaded binary is not executable on unix until it is said to be.
    #[cfg(unix)]
    make_executable(&staging);

    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&staging, &dest)
        .map_err(|e| format!("failed to install into {}: {e}", dest.display()))?;

    // Confirm the thing we just unpacked is actually reachable, rather than reporting success and
    // letting the next build fail on a layout we guessed wrong.
    let found = match id {
        ToolId::Cmake => cmake_program(),
        _ => ninja_program(),
    }
    .filter(|p| is_hub_installed(p))
    .ok_or_else(|| {
        format!(
            "unpacked into {}, but no executable turned up where one was expected",
            dest.display()
        )
    })?;

    Ok(found.to_string_lossy().into_owned())
}

/// Mark everything under `dir` executable, for archives that carry no permissions.
#[cfg(unix)]
fn make_executable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            make_executable(&path);
        } else if let Ok(meta) = path.metadata() {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
}

struct Asset {
    name: String,
    url: String,
    size: u64,
}

/// The newest non-prerelease asset matching this download's platform pattern.
fn latest_asset(download: &Download) -> Result<Asset, String> {
    #[derive(serde::Deserialize)]
    struct Release {
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        assets: Vec<ReleaseAsset>,
    }
    #[derive(serde::Deserialize)]
    struct ReleaseAsset {
        name: String,
        #[serde(default)]
        size: u64,
        browser_download_url: String,
    }

    let url = format!("https://api.github.com/repos/{}/releases?per_page=10", download.repo);
    let releases: Vec<Release> = framework::http()?
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("could not reach GitHub to look up {}: {e}", download.repo))?;

    releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .flat_map(|r| r.assets)
        // Both the platform pattern and, for CMake, the extension: the release carries a dozen
        // assets and only one is this host's.
        .find(|a| download.matches.iter().all(|m| a.name.contains(m)))
        .map(|a| Asset { name: a.name, url: a.browser_download_url, size: a.size })
        .ok_or_else(|| {
            format!(
                "no {} release publishes a build for this platform ({})",
                download.repo,
                download.matches.join(" + ")
            )
        })
}

// --- Handing off to a compiler installer --------------------------------------------------

/// Official Microsoft bootstrapper for the standalone Build Tools.
#[cfg(windows)]
const VS_BUILD_TOOLS: &str = "https://aka.ms/vs/17/release/vs_BuildTools.exe";

/// Start the platform's own compiler installer.
///
/// Deliberately *their* installer, run interactively. The licence is accepted in Microsoft's UI and
/// the elevation prompt is Windows', not a dialog the Hub invented — the Hub only saves the user
/// finding the right download and ticking the right workload.
pub fn install_compiler() -> Result<String, String> {
    #[cfg(windows)]
    {
        let bootstrapper = tools_dir().join("vs_BuildTools.exe");
        if !bootstrapper.is_file() {
            std::fs::create_dir_all(tools_dir()).map_err(|e| e.to_string())?;
            let bytes = framework::http()?
                .get(VS_BUILD_TOOLS)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.bytes())
                .map_err(|e| format!("failed to download Microsoft's installer: {e}"))?;
            std::fs::write(&bootstrapper, &bytes)
                .map_err(|e| format!("failed to save the installer: {e}"))?;
        }

        // No --quiet: the user sees Microsoft's UI, accepts Microsoft's licence, and approves the
        // elevation themselves. The workload is preselected because that is the part people get
        // wrong — a Visual Studio without "Desktop development with C++" has no compiler.
        std::process::Command::new(&bootstrapper)
            .args([
                "--add",
                "Microsoft.VisualStudio.Workload.VCTools",
                "--includeRecommended",
            ])
            .spawn()
            .map_err(|e| format!("failed to start Microsoft's installer: {e}"))?;
        return Ok("Microsoft's installer is starting. Re-run this check once it finishes.".into());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("xcode-select")
            .arg("--install")
            .spawn()
            .map_err(|e| format!("failed to start Apple's installer: {e}"))?;
        return Ok("Apple's installer is starting. Re-run this check once it finishes.".into());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Err(format!(
            "Installing a compiler needs your package manager and a root password, which the Hub \
             will not ask for. Run:\n\n    {}",
            linux_install_command()
        ))
    }
}

/// True when a build could actually run right now — everything but vcpkg, which is only needed by a
/// project that declares libraries.
pub fn ready() -> bool {
    status()
        .iter()
        .filter(|t| t.id != ToolId::Vcpkg)
        .all(|t| t.found())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever this machine has, the wizard must be able to describe it: every row filled in, and
    /// nothing claiming to be both missing and found.
    #[test]
    fn every_dependency_reports_a_coherent_state() {
        for tool in status() {
            assert!(!tool.name.is_empty(), "{:?} needs a name", tool.id);
            assert!(!tool.purpose.is_empty(), "{:?} needs a purpose", tool.id);
            if tool.source == Source::Missing {
                assert!(tool.path.is_none(), "{:?} is missing but has a path", tool.id);
                // Something has to tell the user what to do — either the Hub installs it, or the
                // guidance explains who does.
                assert!(
                    tool.installable || tool.guidance.is_some(),
                    "{:?} is missing with no way forward",
                    tool.id
                );
            } else {
                assert!(tool.path.is_some(), "{:?} was found but reports no path", tool.id);
            }
        }
    }

    /// Print what this machine actually resolves, which is the fastest way to see what the wizard
    /// will show. Ignored: it asserts nothing about a machine it cannot know.
    #[test]
    #[ignore]
    fn show_this_machine() {
        for tool in status() {
            println!(
                "{:<10} {:?}\n           path:    {}\n           version: {}\n           note:    {}",
                tool.name,
                tool.source,
                tool.path.unwrap_or_else(|| "-".into()),
                tool.version.unwrap_or_else(|| "-".into()),
                tool.guidance.unwrap_or_else(|| "-".into()),
            );
        }
        println!("ready to build: {}", ready());
    }
    /// The wizard reads these fields by name, so the serialized shape is a contract with the
    /// frontend: camelCase keys, and `source` as one of the four strings its union allows. A
    /// silent rename here would show up as a row of blanks, not as an error.
    #[test]
    fn the_serialized_shape_matches_what_the_ui_expects() {
        let json = serde_json::to_value(status()).unwrap();
        let rows = json.as_array().expect("a list of tools");
        assert_eq!(rows.len(), 4);

        for row in rows {
            for key in ["id", "name", "purpose", "source", "installable"] {
                assert!(row.get(key).is_some(), "missing {key} in {row}");
            }
            let source = row["source"].as_str().unwrap();
            assert!(
                matches!(source, "missing" | "system" | "bundled" | "hub"),
                "unexpected source {source:?}"
            );
            let id = row["id"].as_str().unwrap();
            assert!(
                matches!(id, "cmake" | "ninja" | "compiler" | "vcpkg"),
                "unexpected id {id:?}"
            );
            // Absent rather than null, so the optional fields match `field?: string` in TypeScript.
            for key in ["path", "version", "guidance", "handoff"] {
                assert!(!row[key].is_null() || row.get(key).is_none(), "{key} should be absent, not null");
            }
        }
    }
    /// Actually download and unpack a tool, which is the only way to know the asset pattern matches
    /// a real release and the archive lays out where `cmake_program`/`ninja_program` look.
    ///
    /// Ignored: it hits GitHub and writes into the Hub's data directory. Run one with
    /// `cargo test --lib toolchain::tests::really_install -- --ignored --nocapture`, choosing with
    /// `KORAL_INSTALL_TOOL=ninja` (or `cmake`).
    #[test]
    #[ignore]
    fn really_install() {
        let which = std::env::var("KORAL_INSTALL_TOOL").unwrap_or_else(|_| "ninja".into());
        let id = match which.as_str() {
            "cmake" => ToolId::Cmake,
            _ => ToolId::Ninja,
        };
        let mut last = 0;
        let path = install(id, |downloaded, total| {
            if total > 0 && downloaded * 100 / total >= last + 25 {
                last = downloaded * 100 / total;
                println!("  {last}%");
            }
        })
        .expect("install should succeed");
        println!("installed {which} to {path}");

        // Reported as Hub-installed, and able to answer for itself.
        let tool = status().into_iter().find(|t| t.id == id).unwrap();
        assert_eq!(tool.source, Source::Hub, "a Hub install must report as such");
        println!("version: {:?}", tool.version);
        assert!(tool.version.is_some(), "the installed tool should report a version");
    }
    /// The compiler is never installable by the Hub, on any platform. If this ever flips, the
    /// wizard would offer a button that accepts someone else's licence.
    #[test]
    fn the_compiler_is_never_something_the_hub_installs() {
        let compiler = status().into_iter().find(|t| t.id == ToolId::Compiler).unwrap();
        assert!(!compiler.installable);
    }

    /// Each platform must resolve exactly one asset pattern, or `install` would pick a build for
    /// the wrong operating system.
    #[test]
    fn the_download_patterns_are_platform_specific() {
        let cmake = cmake_download().expect("cmake is installable everywhere the Hub runs");
        let ninja = ninja_download().expect("ninja is installable everywhere the Hub runs");
        assert!(!cmake.matches.is_empty() && !ninja.matches.is_empty());
        // Ninja ships a bare binary; CMake ships a versioned folder that has to be unwrapped.
        assert!(cmake.strip_root);
        assert!(!ninja.strip_root);
    }
}
