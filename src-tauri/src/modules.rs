//! The module registry: which Koral modules this machine can offer a project.
//!
//! "Registered" means visible to the Hub — a module-kind project on the recent list (created,
//! imported, or downloaded from a collection), or a module that ships inside an installed SDK.
//! The settings panel offers these as a menu, and what it writes into a project's `"modules"`
//! list is the exact string the *runtime* resolves:
//!
//!   - a project module contributes its `name` verbatim — the runtime decorates it per platform
//!     (`MyCameras` → `libMyCameras.so`), which is exactly the file that project's CMake builds;
//!   - an SDK module contributes its library's bare stem (`koral-camera`), which the runtime
//!     finds beside the installed framework on its own.
//!
//! A project module's library lives in that project's build tree, which the runtime does not
//! search. [`stage`] is the bridge: after a build, every referenced project module is copied in
//! next to the scene library — a directory the runtime *does* search — so the Hub's ▶ works, and
//! so does an IDE run afterwards, since it launches the very same library layout.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::builder;
use crate::framework;
use crate::model::Kind;
use crate::project;

/// One module the picker can offer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleView {
    /// The exact string to store under `"modules"` in koral.json.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Where it comes from: `"project"` (a module project on this machine) or `"framework"`
    /// (shipped inside an installed SDK).
    pub source: &'static str,
    /// The project root, for project modules — so the UI can say which folder it means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether this machine has the module's source, so a debugger can step into it. Always true
    /// for a module project (the Hub builds it from that source); true for a framework module only
    /// when the framework itself is a build from source.
    pub has_source: bool,
}

/// Every module-kind project on the recent list, with its root.
fn project_modules() -> Vec<(String, PathBuf)> {
    project::recent_paths()
        .into_iter()
        .filter_map(|path| {
            let cfg = project::load(&path).ok()?;
            (cfg.kind == Kind::Module).then(|| (cfg.name, path))
        })
        .collect()
}

/// The module project registered under this exact name, if any. What the build/run staging step
/// uses to turn a `"modules"` entry back into a buildable project.
pub fn find_project_module(name: &str) -> Option<PathBuf> {
    project_modules()
        .into_iter()
        .find_map(|(n, path)| (n == name).then_some(path))
}

/// The bare module name a library file stands for, or `None` for a file that is not a shared
/// library. The inverse of the runtime's per-platform decoration.
fn bare_name(file: &Path) -> Option<String> {
    let stem = file.file_stem()?.to_str()?;
    match file.extension()?.to_str()? {
        "dll" => Some(stem.to_string()),
        "so" | "dylib" => Some(stem.strip_prefix("lib").unwrap_or(stem).to_string()),
        _ => None,
    }
}

/// What a project's checked modules contribute to *its build*, as distinct from its runtime.
///
/// A module the project has not checked contributes nothing here, which is the whole point: a
/// module is a separate project, and its headers have no business being visible to code that
/// never asked for it. Unchecking one should break the build that uses it, not defer the failure
/// to a load-time "undefined symbol".
pub struct BuildInputs {
    /// Exported SDK targets to link (`Koral::koral-camera`). Linking is what carries the module's
    /// include directory in — CMake propagates it through the target's usage requirements — so
    /// nothing here has to know where the SDK keeps its headers.
    pub sdk_targets: Vec<String>,
    /// Header directories for module projects on this machine. Absolute, and therefore
    /// machine-local: the caller must keep these out of the committed CMakeLists.
    pub include_dirs: Vec<PathBuf>,
}

/// Split a project's `"modules"` list into what its build needs.
///
/// Entries that are neither a registered module project nor a module the SDK ships — a bare path,
/// or a name meant for a machine that has it — contribute nothing and are left to the runtime,
/// which is the only thing that can resolve them.
pub fn build_inputs(entries: &[String], sdk_root: &Path) -> BuildInputs {
    let shipped = scan_sdk_modules(sdk_root);
    let mut inputs = BuildInputs { sdk_targets: Vec::new(), include_dirs: Vec::new() };

    for entry in entries {
        if let Some(root) = find_project_module(entry) {
            // A module project's public header is its interface, and it lives in `src` beside the
            // implementation — the layout `project::write_sources` scaffolds.
            let src = root.join("src");
            if src.is_dir() {
                inputs.include_dirs.push(src);
            }
            continue;
        }
        if shipped.iter().any(|name| name == entry) {
            inputs.sdk_targets.push(format!("Koral::{entry}"));
        }
    }
    inputs
}

/// The module names an unpacked SDK at `sdk_root` ships.
///
/// `lib/modules` on Linux/macOS, `bin/modules` on Windows — mirroring where the SDK's install
/// rules put them, which in turn mirrors where the *runtime* looks (a `modules/` directory beside
/// libKoral). These three have to agree or a shipped module is invisible, so this is pinned by a
/// test rather than left as a comment.
fn scan_sdk_modules(sdk_root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for dir in ["lib/modules", "bin/modules"] {
        let Ok(entries) = std::fs::read_dir(sdk_root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(name) = bare_name(&entry.path()) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Modules shipped inside the installed SDK for `framework_version`. Installed only — opening a
/// settings panel must never hit the network, so a version that is not downloaded yet simply
/// contributes nothing. Understands source pins, whose SDK is a directory the user built.
fn framework_modules(framework_version: &str) -> Vec<String> {
    framework::installed_root(framework_version)
        .map(|root| scan_sdk_modules(&root))
        .unwrap_or_default()
}

/// Everything the picker offers for a project targeting `framework_version`: the user's own
/// module projects first, then the SDK's. A name claimed by both is offered once, as the user's —
/// the runtime searches the staged copy (beside the scene library) before the SDK's directory,
/// so the user's version is also the one that would actually load.
pub fn list_available(framework_version: &str) -> Vec<ModuleView> {
    let mut out: Vec<ModuleView> = project_modules()
        .into_iter()
        .map(|(name, path)| ModuleView {
            id: name.clone(),
            name,
            source: "project",
            path: Some(path.to_string_lossy().into_owned()),
            // The Hub builds this project itself, from sources on this machine.
            has_source: true,
        })
        .collect();

    // A framework module's source only exists here when the framework itself was built here.
    let framework_has_source = !framework::debug_source_dirs(framework_version).is_empty();
    for name in framework_modules(framework_version) {
        if out.iter().any(|m| m.id == name) {
            continue;
        }
        out.push(ModuleView {
            id: name.clone(),
            name,
            source: "framework",
            path: None,
            has_source: framework_has_source,
        });
    }
    out
}

/// Source directories a debugger should search for the modules in a project's `"modules"` list.
///
/// Only the ones that are module *projects* on this machine contribute: the Hub built those, so
/// their debug info points at sources that are right here. An SDK module's source lives in the
/// framework tree, and is covered by [`framework::debug_source_dirs`] instead.
pub fn debug_source_dirs(entries: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for entry in entries {
        let Some(root) = find_project_module(entry) else {
            continue;
        };
        let src = root.join("src");
        if src.is_dir() {
            dirs.push(src);
        }
        dirs.push(root);
    }
    dirs
}

/// The build directories holding the module libraries a project loads, so a debugger can find
/// their symbols where they were actually built (the staged copies beside the scene library carry
/// the same debug info, but the originals are what an IDE's own build refreshes).
pub fn debug_library_dirs(entries: &[String], profile: &str) -> Vec<PathBuf> {
    entries
        .iter()
        .filter_map(|entry| find_project_module(entry))
        .map(|root| root.join(crate::scaffold::build_dir_name(profile)))
        .collect()
}

/// Copy each referenced project module's built library in beside the scene library.
///
/// `entries` is the project's `"modules"` list; names that resolve to a registered module project
/// are staged, everything else (SDK modules, paths, hand-written names) is left to the runtime's
/// own search. The caller has already built those module projects — a missing library here means
/// that build was skipped, and is reported as exactly that.
pub fn stage(entries: &[String], module_roots: &[(String, PathBuf)], dest: &Path, profile: &str)
    -> Result<(), String>
{
    for entry in entries {
        let Some((_, root)) = module_roots.iter().find(|(name, _)| name == entry) else {
            continue;
        };
        let lib = root
            .join(crate::scaffold::build_dir_name(profile))
            .join(builder::lib_file_name(entry));
        if !lib.is_file() {
            return Err(format!(
                "module '{entry}' has no built library at {} — build that project first",
                lib.display()
            ));
        }
        let target = dest.join(builder::lib_file_name(entry));
        std::fs::copy(&lib, &target).map_err(|e| {
            format!("failed to stage module '{entry}' into {}: {e}", dest.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Staging is what makes a module project's library reachable from the scene library's
    /// directory. Entries that are not registered module projects must be left alone — those are
    /// the SDK's, and the runtime finds them itself.
    #[test]
    fn stage_copies_project_modules_and_ignores_the_rest() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-stage-test-{n}"));
        let module_root = base.join("MyCameras");
        let build = module_root.join(crate::scaffold::build_dir_name("Debug"));
        let dest = base.join("game/cmake-build-debug");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(build.join(builder::lib_file_name("MyCameras")), "not really a library")
            .unwrap();

        let roots = vec![("MyCameras".to_string(), module_root.clone())];
        let entries = vec!["MyCameras".to_string(), "koral-camera".to_string()];
        stage(&entries, &roots, &dest, "Debug").unwrap();

        assert!(dest.join(builder::lib_file_name("MyCameras")).is_file());
        assert!(
            !dest.join(builder::lib_file_name("koral-camera")).exists(),
            "an SDK module must not be staged — the runtime finds it beside the framework"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// A registered module whose library was never built names the file it looked for, rather
    /// than letting the launch fail later with the runtime's "module not found".
    #[test]
    fn stage_reports_an_unbuilt_module() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-stage-missing-{n}"));
        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let roots = vec![("MyCameras".to_string(), base.join("MyCameras"))];
        let err = stage(&["MyCameras".to_string()], &roots, &dest, "Debug").unwrap_err();
        assert!(err.contains("MyCameras"), "{err}");
        assert!(err.contains("build that project first"), "{err}");

        std::fs::remove_dir_all(&base).ok();
    }

    /// The SDK layout this scan depends on, pinned against drift: the engine installs its modules
    /// into `<prefix>/lib/modules` (`bin/modules` on Windows), and a real `cmake --install` of the
    /// engine was checked to produce exactly `lib/modules/libkoral-camera.so`. If that install rule
    /// ever moves, this test is what fails instead of the picker quietly going empty.
    #[test]
    fn sdk_modules_are_found_in_the_installed_layout() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let sdk = std::env::temp_dir().join(format!("koral-sdkscan-{n}"));
        std::fs::create_dir_all(sdk.join("lib/modules")).unwrap();
        std::fs::create_dir_all(sdk.join("bin/modules")).unwrap();
        std::fs::create_dir_all(sdk.join("lib")).unwrap();

        std::fs::write(sdk.join("lib/modules/libkoral-camera.so"), "").unwrap();
        std::fs::write(sdk.join("bin/modules/koral-physics.dll"), "").unwrap();
        // Not modules: the SDK's own library, and anything that isn't a shared object.
        std::fs::write(sdk.join("lib/libKoral.so"), "").unwrap();
        std::fs::write(sdk.join("lib/modules/README.txt"), "").unwrap();

        assert_eq!(
            scan_sdk_modules(&sdk),
            vec!["koral-camera".to_string(), "koral-physics".to_string()]
        );

        // An SDK with no modules directory at all (every release before modules existed) is
        // simply a framework that contributes none — never an error.
        let bare = std::env::temp_dir().join(format!("koral-sdkscan-bare-{n}"));
        std::fs::create_dir_all(&bare).unwrap();
        assert!(scan_sdk_modules(&bare).is_empty());

        std::fs::remove_dir_all(&sdk).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    #[test]
    fn bare_names_invert_the_platform_decoration() {
        assert_eq!(bare_name(Path::new("libkoral-camera.so")), Some("koral-camera".into()));
        assert_eq!(bare_name(Path::new("libMyCameras.dylib")), Some("MyCameras".into()));
        assert_eq!(bare_name(Path::new("koral-camera.dll")), Some("koral-camera".into()));
        assert_eq!(bare_name(Path::new("framework.json")), None);
        assert_eq!(bare_name(Path::new("notes.txt")), None);
    }
}
