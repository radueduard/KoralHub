//! The Hub's own vcpkg checkout, and the catalogue of ports a project can pick from.
//!
//! **One clone per machine, owned entirely by the Hub.** A project that declares `libraries` needs
//! a package manager to resolve them, and the Hub's whole premise is that nothing has to be
//! installed by hand — so it clones vcpkg itself, under its data directory, and hands CMake that
//! toolchain file. `$VCPKG_ROOT` still wins when the user has set one, because someone who
//! maintains their own vcpkg almost certainly means to use it.
//!
//! The clone is **shallow** (`--depth 1`), which is the difference between roughly 250 MB and well
//! over a gigabyte. Nothing here reads history: the port tree at the tip is the catalogue, and
//! `update` force-resets onto whatever the next fetch brings. (`git::clone_shallow` falls back to a
//! full clone if a transport refuses the depth; that costs disk, not correctness.) It also means no
//! `builtin-baseline` — a baseline names a commit a shallow clone does not have — which costs
//! nothing today, since the SDK publishes no baseline for projects to pin against either (see
//! `FrameworkManifest::vcpkg_baseline`).
//!
//! Cloning happens in the background on the first run that has no checkout, so startup is never
//! held up by it; [`Progress`] is what the UI watches. Updating is on demand, never automatic — a
//! port tree that moved under a project between two builds is exactly the kind of surprise this
//! design is trying to avoid.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::paths;

/// Where the port tree comes from.
const VCPKG_REPO: &str = "https://github.com/microsoft/vcpkg.git";

/// True while a clone or update is running, so a second one is never started on top of it.
static BUSY: AtomicBool = AtomicBool::new(false);

/// One vcpkg port, as the library picker shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Port {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Optional feature names, for the ports that have them (`imgui[docking-experimental]`).
    #[serde(default)]
    pub features: Vec<String>,
    /// What to pass `find_package()` to bring this port in. Rarely the port's own name —
    /// `nlohmann-json` is found as `nlohmann_json`, `entt` as `EnTT`.
    #[serde(default)]
    pub packages: Vec<String>,
    /// What to link, verbatim, including any generator expression the port recommends.
    #[serde(default)]
    pub targets: Vec<String>,
    /// True when the two above were *inferred* from the port name rather than read from the
    /// port's `usage` file. Only about a fifth of ports ship one, and a guess that is wrong makes
    /// `find_package` fail — so the UI has to be able to say "check this" and let it be corrected.
    #[serde(default)]
    pub guessed: bool,
}

/// What the UI needs to know about the checkout before it can offer anything.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// A usable port tree is on disk.
    pub ready: bool,
    /// A clone or update is running right now.
    pub busy: bool,
    /// Absolute path of the checkout, so the UI can say where the disk went.
    pub path: String,
    /// How many ports the index holds. Zero until the first index is built.
    pub port_count: usize,
    /// Short commit id of the port tree, so "update" has something to show for itself.
    pub revision: String,
    /// Set when the Hub is *not* the one providing vcpkg: the `$VCPKG_ROOT` the user set, which
    /// takes precedence. The UI says so rather than offering to manage a tree it does not own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_root: Option<String>,
}

/// A `$VCPKG_ROOT` the user set themselves, when it points at something real.
///
/// Respected ahead of the Hub's own clone: a machine that already maintains a vcpkg — pinned to a
/// baseline, with a binary cache warmed up — should not have a second copy imposed on it.
fn external_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var("VCPKG_ROOT").ok()?);
    root.join("scripts/buildsystems/vcpkg.cmake")
        .is_file()
        .then_some(root)
}

/// The checkout the Hub manages, whether or not it exists yet.
pub fn hub_root() -> PathBuf {
    paths::vcpkg_dir()
}

/// The vcpkg to build against: the user's, or the Hub's own once it has been cloned.
pub fn root() -> Option<PathBuf> {
    if let Some(external) = external_root() {
        return Some(external);
    }
    let hub = hub_root();
    is_checkout(&hub).then_some(hub)
}

/// Does this directory hold a vcpkg checkout the toolchain can be pointed at?
///
/// Both files are checked because a clone interrupted half way leaves a directory that exists and
/// is useless; the toolchain file is what CMake actually consumes, and `ports/` is what the picker
/// reads.
fn is_checkout(dir: &Path) -> bool {
    dir.join("scripts/buildsystems/vcpkg.cmake").is_file() && dir.join("ports").is_dir()
}

/// Absolute path of the CMake toolchain file, or `None` when no vcpkg is available yet.
pub fn toolchain_file() -> Option<PathBuf> {
    root().map(|r| r.join("scripts/buildsystems/vcpkg.cmake"))
}

pub fn status() -> Status {
    let external = external_root();
    let path = external.clone().unwrap_or_else(hub_root);
    let index = load_index();
    Status {
        ready: is_checkout(&path),
        busy: BUSY.load(Ordering::SeqCst),
        port_count: index.ports.len(),
        revision: index.revision,
        external_root: external.map(|p| p.to_string_lossy().into_owned()),
        path: path.to_string_lossy().into_owned(),
    }
}

// --- The port index -------------------------------------------------------------------

/// Bumped whenever [`Port`] gains a field the index has to be rebuilt to fill in.
///
/// Without this an index written by an older Hub deserializes cleanly — every new field takes its
/// serde default — and the missing data looks like real data. That is exactly how a port ends up
/// recorded with no CMake package name and nothing saying so.
const INDEX_VERSION: u32 = 2;

#[derive(Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    version: u32,
    /// Short commit id the index was built from, so a stale one can be spotted after an update.
    #[serde(default)]
    revision: String,
    #[serde(default)]
    ports: Vec<Port>,
}

fn load_index() -> Index {
    let index: Index = std::fs::read_to_string(paths::vcpkg_index_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // An index from an older Hub is not partially useful, it is misleading — discard it and let
    // the caller rebuild.
    if index.version == INDEX_VERSION {
        index
    } else {
        Index::default()
    }
}

fn save_index(index: &Index) -> Result<(), String> {
    let file = paths::vcpkg_index_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(index).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Every port in the checkout, alphabetically.
///
/// Served from the cached index; [`reindex`] is what rebuilds it, after a clone or an update.
/// Reading it back costs one file rather than the ~2500 small manifests the tree holds, which is
/// the difference between a picker that opens instantly and one that stutters every time.
pub fn ports() -> Vec<Port> {
    let index = load_index();
    if !index.ports.is_empty() {
        return index.ports;
    }
    // No index yet but a checkout is there — a Hub that was killed between the clone and the
    // index, or a `$VCPKG_ROOT` the user pointed at after the fact. Build it now.
    match root() {
        Some(root) if is_checkout(&root) => reindex(&root).map(|i| i.ports).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Read every `ports/*/vcpkg.json` and cache the result.
fn reindex(root: &Path) -> Result<Index, String> {
    let dir = root.join("ports");
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("failed to read {}: {e}", dir.display()))?;

    let mut ports: Vec<Port> = entries
        .flatten()
        .filter_map(|entry| read_port(&entry.path()))
        .collect();
    ports.sort_by(|a, b| a.name.cmp(&b.name));

    let index = Index { version: INDEX_VERSION, revision: revision(root), ports };
    save_index(&index)?;
    Ok(index)
}

/// What `find_package()` and `target_link_libraries()` need for a port, looked up on this machine.
///
/// The answer for a given port is a fact about the port, not about the project — so a project that
/// does not record it (written before the fields existed, or edited by hand) is not left to a guess
/// from the port name while the port tree sitting right here says otherwise. `entt` is exactly that
/// case: its usage file names `EnTT`, and `find_package(entt)` cannot find `EnTTConfig.cmake`.
///
/// `None` when there is no checkout, the port is unknown, or the port ships no usage file — the
/// caller then has nothing better than the port name to go on.
pub fn cmake_names(port: &str) -> Option<(Vec<String>, Vec<String>)> {
    // The index first: it is one file read, and already holds the parsed answer.
    if let Some(found) = load_index().ports.iter().find(|p| p.name == port) {
        if !found.guessed && !found.packages.is_empty() {
            return Some((found.packages.clone(), found.targets.clone()));
        }
        if found.guessed {
            return None; // the index's own answer is the guess we are trying to improve on
        }
    }

    // No index (or a port added since it was built): read the port's usage file directly.
    let root = root()?;
    let usage = std::fs::read_to_string(root.join("ports").join(port).join("usage")).ok()?;
    let (packages, targets) = parse_usage(&usage);
    (!packages.is_empty()).then_some((packages, targets))
}

/// One port directory's manifest, reduced to what the picker shows.
///
/// Tolerant on purpose: a manifest the Hub cannot read is skipped rather than failing the whole
/// index. The tree carries a few thousand of them written over many years, and one odd file must
/// not cost the user the catalogue.
fn read_port(dir: &Path) -> Option<Port> {
    let name = dir.file_name()?.to_string_lossy().into_owned();
    let text = std::fs::read_to_string(dir.join("vcpkg.json")).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;

    // `description` is a string in most manifests and an array of paragraphs in some.
    let description = match doc.get("description") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(lines)) => lines
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };

    // The version key has four spellings, one per versioning scheme; a port uses exactly one.
    let version = ["version", "version-semver", "version-date", "version-string"]
        .iter()
        .find_map(|key| doc.get(*key).and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_string();

    // Features are an object keyed by name in current manifests, and were an array of objects
    // carrying a "name" in older ones.
    let features = match doc.get("features") {
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        Some(serde_json::Value::Array(list)) => list
            .iter()
            .filter_map(|f| f.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    let (packages, targets) = std::fs::read_to_string(dir.join("usage"))
        .ok()
        .map(|usage| parse_usage(&usage))
        .filter(|(packages, _)| !packages.is_empty())
        .unwrap_or_default();

    let guessed = packages.is_empty();
    let (packages, targets) = if guessed {
        // No usage file — about four ports in five. The port name is the best available guess and
        // is right often enough to be worth making (imgui, fmt, glm), but it is a guess, and
        // `guessed` is what makes the UI say so instead of quietly emitting a bad find_package.
        (vec![name.clone()], vec![format!("{name}::{name}")])
    } else {
        (packages, targets)
    };

    Some(Port { name, version, description, features, packages, targets, guessed })
}

/// Pull the CMake incantation out of a port's `usage` file.
///
/// These are prose written for a human to copy, not a machine-readable field, so this reads them
/// the way a human does — and stops at the *first* recipe. That matters: several ports document
/// alternatives one after another (fmt offers `fmt::fmt` and then, as a second option,
/// `fmt::fmt-header-only`), and taking every snippet would link both halves of an either/or.
///
/// Everything inside `target_link_libraries` is kept verbatim apart from the target name and the
/// visibility keyword, so a port that recommends a generator expression (sdl2) or a variable
/// (bullet3) gets exactly what it asked for rather than something parsed down to a name.
fn parse_usage(text: &str) -> (Vec<String>, Vec<String>) {
    // Comment lines are where ports put the "or use the header-only version" asides, and where
    // bullet3 lists component names that are not targets.
    let body: String = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");

    let Some(package_args) = first_call(&body, "find_package") else {
        return (Vec::new(), Vec::new());
    };
    let Some(package) = package_args.split_whitespace().next() else {
        return (Vec::new(), Vec::new());
    };

    // A port whose usage documents only include directories (stb) has a package and no targets;
    // that is a real answer, not a parse failure.
    let targets = first_call(&body, "target_link_libraries")
        .map(|args| {
            args.split_whitespace()
                .skip(1) // the consumer's own target, which the usage file calls `main`
                .filter(|token| !matches!(*token, "PRIVATE" | "PUBLIC" | "INTERFACE"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    (vec![package.to_string()], targets)
}

/// The argument text of the first `name(...)` call in `text`, spanning lines, or `None`.
///
/// Balances parentheses so a nested call inside the arguments does not end the match early. The
/// generator expressions these files carry (`$<IF:$<TARGET_EXISTS:X>,X,Y>`) nest with angle
/// brackets rather than parens, so they need no special handling.
fn first_call(text: &str, name: &str) -> Option<String> {
    let at = text.find(&format!("{name}("))?;
    let rest = &text[at + name.len() + 1..];

    let mut depth = 1usize;
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    Some(rest[..end].to_string())
}

/// The CMake names for a port read from a tree vcpkg has actually installed it into.
///
/// The last word for a port that ships no `usage` file. vcpkg installs into
/// `<build>/vcpkg_installed/<triplet>/share/<port>/`, and what lands there is not a guess: the
/// config file's own name is what `find_package` has to be given, and the imported targets are
/// spelled out in `*Targets.cmake`. `glfw3` is the case that needs this — it exports a target
/// called plainly `glfw`, which nothing about the port name suggests.
///
/// Only available once a configure has run far enough for vcpkg to install (which it does before
/// CMake reaches `find_package`), so this corrects a project rather than getting it right first
/// time. `None` when nothing is installed yet.
pub fn installed_cmake_names(project_root: &Path, port: &str) -> Option<(Vec<String>, Vec<String>)> {
    let share = installed_share_dir(project_root, port)?;

    // `find_package(X)` accepts `XConfig.cmake` or `x-config.cmake`; either way X is the name.
    let mut packages: Vec<String> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&share).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_suffix("Config.cmake").or_else(|| file.strip_suffix("-config.cmake")) {
            if !name.is_empty() && !packages.contains(&name.to_string()) {
                packages.push(name.to_string());
            }
        } else if file.ends_with("Targets.cmake") {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            for target in imported_targets(&text) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
    }

    (!packages.is_empty()).then_some((packages, targets))
}

/// `<build>/vcpkg_installed/<triplet>/share/<port>/`, in whichever of the project's build trees has
/// one. Any of them will do — the port is the same package however it was configured.
fn installed_share_dir(project_root: &Path, port: &str) -> Option<PathBuf> {
    let builds = std::fs::read_dir(project_root).ok()?;
    for build in builds.flatten() {
        let installed = build.path().join("vcpkg_installed");
        let Ok(triplets) = std::fs::read_dir(&installed) else {
            continue;
        };
        for triplet in triplets.flatten() {
            let share = triplet.path().join("share").join(port);
            if share.is_dir() {
                return Some(share);
            }
        }
    }
    None
}

/// The imported targets an exported `*Targets.cmake` defines.
///
/// These files declare each one as `add_library(<name> <type> IMPORTED)`, so the target name is
/// the first argument — namespaced (`EnTT::EnTT`) or not (`glfw`), which is exactly the
/// distinction that cannot be guessed. Anything that is not an IMPORTED declaration is skipped.
fn imported_targets(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let args = line.trim().strip_prefix("add_library(")?.strip_suffix(')')?;
            let mut parts = args.split_whitespace();
            let name = parts.next()?;
            // `add_library(a::b ALIAS a)` is not an import, and neither is a real library.
            parts.any(|word| word == "IMPORTED").then(|| name.to_string())
        })
        .collect()
}

/// Short commit id of a checkout, for display only.
fn revision(root: &Path) -> String {
    let Ok(repo) = git2::Repository::open(root) else {
        return String::new();
    };
    repo.head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|c| c.id().to_string().chars().take(7).collect())
        .unwrap_or_default()
}

// --- Getting and keeping the checkout ---------------------------------------------------

/// Progress of a clone or update, reported to whoever asked for it.
pub enum Progress<'a> {
    Step(&'a str),
    Done { ports: usize },
    Failed(&'a str),
}

/// Clone the port tree if this machine has none. Does nothing when a checkout is already there,
/// when the user provides their own `$VCPKG_ROOT`, or when one is already running.
///
/// Called on startup, off the main thread — see `lib.rs`. Failure is not fatal to anything except
/// projects that declare libraries, so it is reported and otherwise swallowed: a machine that is
/// offline on first run should still open the Hub.
pub fn ensure(mut progress: impl FnMut(Progress)) {
    if external_root().is_some() || is_checkout(&hub_root()) {
        // Nothing to fetch, but the index may still be missing (a Hub killed mid-index, or an
        // upgrade from a version that had none).
        if let Some(root) = root() {
            if load_index().ports.is_empty() {
                let count = reindex(&root).map(|i| i.ports.len()).unwrap_or(0);
                progress(Progress::Done { ports: count });
            }
        }
        return;
    }
    run(&mut progress, |root, progress| {
        // A directory left behind by an interrupted clone is not a checkout, and libgit2 refuses
        // to clone into anything non-empty — so start clean.
        if root.exists() {
            std::fs::remove_dir_all(root)
                .map_err(|e| format!("failed to clear {}: {e}", root.display()))?;
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        progress(Progress::Step("Cloning the vcpkg port tree…"));
        crate::git::clone_shallow(VCPKG_REPO, root)?;
        progress(Progress::Step("Bootstrapping vcpkg…"));
        bootstrap(root)
    });
}

/// Fetch vcpkg's own executable, which the toolchain file needs to resolve anything.
///
/// `vcpkg.cmake` would do this itself on the first configure, but then it happens inside a build
/// the user is waiting on, and a failure surfaces as a CMake error about a package manager they
/// never asked for. Doing it here means it happens in the background with the clone, and anything
/// wrong is reported by the Libraries panel where it can be acted on.
///
/// Best-effort by contract: a bootstrap that fails still leaves a usable port tree, and the
/// toolchain's own bootstrap gets a second try at the first build.
fn bootstrap(root: &Path) -> Result<(), String> {
    let script = root.join(if cfg!(windows) {
        "bootstrap-vcpkg.bat"
    } else {
        "bootstrap-vcpkg.sh"
    });
    if !script.is_file() {
        return Ok(()); // a vcpkg layout we don't recognise; let the toolchain deal with it
    }

    // Via `external_command` so it inherits no bundled-library loader environment and flashes no
    // console window on Windows — the same treatment cmake and the compiler get.
    let output = crate::builder::external_command(&script)
        // Telemetry is opt-out, and a package manager the Hub installed on the user's behalf is
        // not a thing they agreed to be measured by.
        .arg("-disableMetrics")
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run {}: {e}", script.display()))?;

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        let details = details.trim();
        let tail = details.lines().rev().take(3).collect::<Vec<_>>().join(" ");
        return Err(format!("vcpkg bootstrap failed ({}): {tail}", output.status));
    }
    Ok(())
}

/// Fetch the port tree and force it onto origin, then rebuild the index. On demand only.
///
/// Overwrite, like every other update in the Hub: the checkout is the Hub's, nobody edits it, and
/// a merge is not a thing that can meaningfully happen to it.
pub fn update(mut progress: impl FnMut(Progress)) {
    if external_root().is_some() {
        progress(Progress::Failed(
            "vcpkg comes from your own $VCPKG_ROOT here, so the Hub does not update it",
        ));
        return;
    }
    if !is_checkout(&hub_root()) {
        // Never cloned (or a failed one): getting it is the update.
        ensure(progress);
        return;
    }
    run(&mut progress, |root, progress| {
        progress(Progress::Step("Fetching the latest ports…"));
        crate::git::update_from_origin(root)?;
        // The tool version is pinned by the tree, so a fetch can move it; re-running the
        // bootstrap is what picks that up. It is a no-op when nothing changed.
        progress(Progress::Step("Bootstrapping vcpkg…"));
        bootstrap(root)
    });
}

/// Shared scaffolding for clone and update: hold the busy flag, do the work, reindex, report.
fn run(
    progress: &mut impl FnMut(Progress),
    work: impl FnOnce(&Path, &mut dyn FnMut(Progress)) -> Result<(), String>,
) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return; // already cloning or updating; the caller will hear about that one
    }
    let root = hub_root();
    let result = work(&root, &mut |p| progress(p)).and_then(|_| {
        progress(Progress::Step("Indexing ports…"));
        reindex(&root)
    });
    BUSY.store(false, Ordering::SeqCst);

    match result {
        Ok(index) => progress(Progress::Done { ports: index.ports.len() }),
        Err(e) => progress(Progress::Failed(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("koral-vcpkg-{tag}-{n}"))
    }

    fn write_port(ports: &Path, name: &str, manifest: &str) {
        let dir = ports.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vcpkg.json"), manifest).unwrap();
    }

    /// The port tree spans years of manifest schema changes, and the picker has to read all of
    /// them: description as a string or as paragraphs, four spellings of the version key, features
    /// as an object or as the older array. A port whose manifest cannot be read at all is skipped
    /// rather than costing the user the rest of the catalogue.
    #[test]
    fn ports_are_read_across_every_manifest_shape() {
        let root = scratch("ports");
        let ports = root.join("ports");
        std::fs::create_dir_all(&ports).unwrap();

        write_port(&ports, "glm", r#"{"name":"glm","version":"1.0.1","description":"Header only maths"}"#);
        write_port(
            &ports,
            "imgui",
            r#"{"name":"imgui","version-semver":"1.91.0",
                "description":["Bloat-free UI","for C++"],
                "features":{"docking-experimental":{"description":"docking"},"vulkan-binding":{}}}"#,
        );
        write_port(
            &ports,
            "old-style",
            r#"{"name":"old-style","version-string":"2020-01-01",
                "features":[{"name":"extra","description":"more"}]}"#,
        );
        write_port(&ports, "broken", "{ not json");
        std::fs::create_dir_all(ports.join("empty")).unwrap();

        let mut found: Vec<Port> = std::fs::read_dir(&ports)
            .unwrap()
            .flatten()
            .filter_map(|e| read_port(&e.path()))
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));

        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["glm", "imgui", "old-style"], "unreadable ports are skipped");

        let imgui = &found[1];
        assert_eq!(imgui.version, "1.91.0");
        assert_eq!(imgui.description, "Bloat-free UI for C++");
        let mut features = imgui.features.clone();
        features.sort();
        assert_eq!(features, ["docking-experimental", "vulkan-binding"]);

        assert_eq!(found[2].version, "2020-01-01");
        assert_eq!(found[2].features, ["extra"]);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The usage files are prose, and these are verbatim from the port tree. Each is here because
    /// it breaks a different naive reading of them.
    #[test]
    fn the_cmake_incantation_is_read_out_of_real_usage_files() {
        // The ordinary case — and the reason a guess from the port name is not good enough:
        // `nlohmann-json` is found as `nlohmann_json`.
        let (packages, targets) = parse_usage(
            "The package nlohmann-json provides CMake targets:\n\n\
             \x20   find_package(nlohmann_json CONFIG REQUIRED)\n\
             \x20   target_link_libraries(main PRIVATE nlohmann_json::nlohmann_json)\n",
        );
        assert_eq!(packages, ["nlohmann_json"]);
        assert_eq!(targets, ["nlohmann_json::nlohmann_json"]);

        // Alternatives, not additions. Linking both fmt::fmt and fmt::fmt-header-only is wrong,
        // so only the first recipe may be taken.
        let (packages, targets) = parse_usage(
            "The package fmt provides CMake targets:\n\n\
             \x20   find_package(fmt CONFIG REQUIRED)\n\
             \x20   target_link_libraries(main PRIVATE fmt::fmt)\n\n\
             \x20   # Or use the header-only version\n\
             \x20   find_package(fmt CONFIG REQUIRED)\n\
             \x20   target_link_libraries(main PRIVATE fmt::fmt-header-only)\n",
        );
        assert_eq!(packages, ["fmt"]);
        assert_eq!(targets, ["fmt::fmt"], "the second snippet is an alternative, not an extra");

        // Multi-line, with generator expressions that must survive intact — parsing them down to
        // a bare target name would drop the static/shared fallback the port is asking for.
        let (packages, targets) = parse_usage(
            "sdl2 provides CMake targets:\n\n\
             \x20   find_package(SDL2 CONFIG REQUIRED)\n\
             \x20   target_link_libraries(main\n\
             \x20       PRIVATE\n\
             \x20       $<TARGET_NAME_IF_EXISTS:SDL2::SDL2main>\n\
             \x20       $<IF:$<TARGET_EXISTS:SDL2::SDL2>,SDL2::SDL2,SDL2::SDL2-static>\n\
             \x20   )\n",
        );
        assert_eq!(packages, ["SDL2"]);
        assert_eq!(
            targets,
            [
                "$<TARGET_NAME_IF_EXISTS:SDL2::SDL2main>",
                "$<IF:$<TARGET_EXISTS:SDL2::SDL2>,SDL2::SDL2,SDL2::SDL2-static>"
            ]
        );

        // A variable rather than a target, and comment lines listing component names that are not
        // targets at all — taking those would produce a link line full of nonsense.
        let (packages, targets) = parse_usage(
            "bullet3 provides CMake targets:\n\n\
             \x20 find_package(Bullet CONFIG REQUIRED)\n\
             \x20 # specific set: BulletSoftBody, BulletDynamics, BulletCollision, LinearMath\n\
             \x20 target_link_libraries(main PRIVATE ${BULLET_LIBRARIES})\n",
        );
        assert_eq!(packages, ["Bullet"]);
        assert_eq!(targets, ["${BULLET_LIBRARIES}"]);

        // Headers by include directory, with nothing to link. A package and no targets is the
        // right answer here, not a failure to parse.
        let (packages, targets) = parse_usage(
            "The package stb provides CMake targets:\n\n\
             \x20   find_package(Stb REQUIRED)\n\
             \x20   target_include_directories(main PRIVATE ${Stb_INCLUDE_DIR})",
        );
        assert_eq!(packages, ["Stb"]);
        assert!(targets.is_empty());

        // Prose with no recipe in it at all.
        assert_eq!(parse_usage("See the upstream docs."), (Vec::new(), Vec::new()));
    }

    /// Four ports in five ship no usage file, so the fallback is the common path — and it has to
    /// be *marked* as a guess, because that is what lets the UI ask the user to check it rather
    /// than emitting a find_package that fails the build with no explanation.
    #[test]
    fn a_port_with_no_usage_file_is_guessed_and_says_so() {
        let root = scratch("guess");
        let ports = root.join("ports");
        std::fs::create_dir_all(&ports).unwrap();

        write_port(&ports, "imgui", r#"{"name":"imgui","version":"1.92.8"}"#);
        let port = read_port(&ports.join("imgui")).unwrap();
        assert!(port.guessed);
        assert_eq!(port.packages, ["imgui"]);
        assert_eq!(port.targets, ["imgui::imgui"]);

        // With a usage file, nothing is guessed.
        write_port(&ports, "entt", r#"{"name":"entt","version":"3.15.0"}"#);
        std::fs::write(
            ports.join("entt/usage"),
            "entt provides CMake targets:\n\n\
             \x20   find_package(EnTT CONFIG REQUIRED)\n\
             \x20   target_link_libraries(main PRIVATE EnTT::EnTT)\n",
        )
        .unwrap();
        let port = read_port(&ports.join("entt")).unwrap();
        assert!(!port.guessed);
        assert_eq!(port.packages, ["EnTT"]);
        assert_eq!(port.targets, ["EnTT::EnTT"]);

        std::fs::remove_dir_all(&root).ok();
    }

    /// An index written by an older Hub deserializes perfectly well — every field added since just
    /// takes its serde default — so missing data is indistinguishable from real data unless the
    /// version is checked. That is precisely how a port came to be recorded with no CMake package
    /// name and a `guessed` flag of false, which is the worst of both: wrong, and not flagged.
    #[test]
    fn an_index_from_an_older_hub_is_discarded_rather_than_half_believed() {
        let old = serde_json::json!({
            "revision": "abc1234",
            "ports": [{ "name": "entt", "version": "3.15.0", "description": "", "features": [] }]
        });
        let parsed: Index = serde_json::from_str(&old.to_string()).unwrap();

        // It *does* parse — that is the trap. The port comes back claiming, by omission, that it
        // needs no find_package and was not guessed.
        assert_eq!(parsed.ports.len(), 1);
        assert!(parsed.ports[0].packages.is_empty());
        assert!(!parsed.ports[0].guessed);
        assert_ne!(parsed.version, INDEX_VERSION, "the version is what tells them apart");

        // A current index round-trips intact.
        let current = Index {
            version: INDEX_VERSION,
            revision: "abc1234".into(),
            ports: vec![Port {
                name: "entt".into(),
                version: "3.15.0".into(),
                description: String::new(),
                features: Vec::new(),
                packages: vec!["EnTT".into()],
                targets: vec!["EnTT::EnTT".into()],
                guessed: false,
            }],
        };
        let back: Index = serde_json::from_str(&serde_json::to_string(&current).unwrap()).unwrap();
        assert_eq!(back.version, INDEX_VERSION);
        assert_eq!(back.ports[0].packages, ["EnTT"]);
    }

    /// What vcpkg installed is the one source that is not a guess, and it is the only way to learn
    /// that `glfw3` exports a target called plainly `glfw` — nothing about the port name, and no
    /// usage file, says so.
    #[test]
    fn the_installed_package_names_itself() {
        let root = scratch("installed");
        let share = root
            .join("cmake-build-debug/vcpkg_installed/x64-linux/share/glfw3");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(share.join("glfw3Config.cmake"), "x").unwrap();
        std::fs::write(share.join("glfw3ConfigVersion.cmake"), "x").unwrap();
        std::fs::write(
            share.join("glfw3Targets.cmake"),
            "# Generated by CMake\n\
             add_library(glfw SHARED IMPORTED)\n\
             set_target_properties(glfw PROPERTIES INTERFACE_INCLUDE_DIRECTORIES \"...\")\n",
        )
        .unwrap();

        let (packages, targets) = installed_cmake_names(&root, "glfw3").unwrap();
        assert_eq!(packages, ["glfw3"]);
        assert_eq!(targets, ["glfw"], "the target is not derivable from the port name");

        // The shape the interface-only packages take, taken from an installed EnTTTargets.cmake.
        let entt = root.join("cmake-build-debug/vcpkg_installed/x64-linux/share/entt");
        std::fs::create_dir_all(&entt).unwrap();
        std::fs::write(entt.join("EnTTConfig.cmake"), "x").unwrap();
        std::fs::write(entt.join("EnTTTargets.cmake"), "add_library(EnTT::EnTT INTERFACE IMPORTED)\n").unwrap();
        assert_eq!(
            installed_cmake_names(&root, "entt").unwrap(),
            (vec!["EnTT".to_string()], vec!["EnTT::EnTT".to_string()])
        );

        // An ALIAS is not an import, and a real library in some other file is not one either —
        // taking either would put a target that does not exist on the link line.
        assert_eq!(
            imported_targets("add_library(a::b ALIAS a)\nadd_library(mylib STATIC src.c)\n"),
            Vec::<String>::new()
        );

        // Nothing installed yet — the common case on a first build, and not an error.
        assert_eq!(installed_cmake_names(&root, "nothing-here"), None);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A half-finished clone leaves a directory that exists and cannot be built against. Treating
    /// it as ready would hand CMake a toolchain file that is not there.
    #[test]
    fn a_partial_clone_does_not_count_as_a_checkout() {
        let root = scratch("partial");
        std::fs::create_dir_all(root.join("ports")).unwrap();
        assert!(!is_checkout(&root), "no toolchain file yet");

        std::fs::create_dir_all(root.join("scripts/buildsystems")).unwrap();
        std::fs::write(root.join("scripts/buildsystems/vcpkg.cmake"), "x").unwrap();
        assert!(is_checkout(&root));

        std::fs::remove_dir_all(&root).ok();
    }
}
