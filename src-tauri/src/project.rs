//! Project storage: read/write a project's portable `koral.json`, scaffold a new project,
//! and maintain the per-machine recent-projects index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{self, Kind, ProjectConfig};
use crate::paths;

/// Committed, portable project metadata file.
pub const CONFIG_FILE: &str = "koral.json";

// --- Load / save the portable config --------------------------------------------------

pub fn load(project_root: &Path) -> Result<ProjectConfig, String> {
    let file = project_root.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    let mut cfg: ProjectConfig = serde_json::from_str(&text)
        .map_err(|e| format!("failed to parse {}: {e}", file.display()))?;

    // Projects scaffolded before the SDK vendored its own dependencies were seeded with the very
    // ports the SDK provides. Left in place they would pull in a whole vcpkg setup to resolve
    // packages the build already has — the thing `libraries` being empty is supposed to avoid.
    cfg.libraries
        .retain(|l| !model::SDK_PROVIDED_PORTS.contains(&l.vcpkg_port.as_str()));

    Ok(cfg)
}

pub fn save(project_root: &Path, config: &ProjectConfig) -> Result<(), String> {
    std::fs::create_dir_all(project_root).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(project_root.join(CONFIG_FILE), text).map_err(|e| e.to_string())
}

// --- Create a new project -------------------------------------------------------------

/// Scaffold a new project under `location/name` and return its root. Fails if the folder
/// already exists so we never clobber someone's work.
pub fn create(
    location: &Path,
    name: &str,
    framework_version: &str,
    color: [f32; 3],
    kind: Kind,
) -> Result<PathBuf, String> {
    if name.trim().is_empty() {
        return Err("project name cannot be empty".into());
    }
    let root = location.join(name);
    if root.exists() {
        return Err(format!(
            "a folder named '{name}' already exists in {}",
            location.display()
        ));
    }

    for sub in ["src", "assets", "shaders"] {
        std::fs::create_dir_all(root.join(sub)).map_err(|e| e.to_string())?;
    }

    save(&root, &ProjectConfig::new(name, framework_version, color, kind))?;
    write_sources(&root, name, kind)?;
    write_gitignore(&root)?;

    // Make the scaffold a git repo with an initial commit. Best-effort: a project is fine without
    // git, so a failure here (e.g. no identity configured and the fallback somehow unavailable)
    // must not fail creation — just log and carry on.
    if let Err(e) = crate::git::init(&root) {
        eprintln!("koral-hub: git init failed for {}: {e}", root.display());
    }

    Ok(root)
}

/// Scaffold the sources for this kind of library.
///
/// An app exports exactly one entry point — `CreateScene` or `CreateJob` — and the engine decides
/// which path to run by which symbol it finds. Exporting the wrong one for the kind would simply
/// never be picked up. A module instead exports the pair `KORAL_DECLARE_MODULE` defines, and its
/// header is the *interface* other projects compile against, so the split between the two files is
/// the whole lesson the template teaches.
fn write_sources(root: &Path, name: &str, kind: Kind) -> Result<(), String> {
    let src = root.join("src");

    if kind == Kind::Module {
        // The module id consumers look the module up by. Lowercased so "MyCameras" and the
        // library file it decorates to don't disagree about case across platforms.
        let id = name.to_lowercase();
        let header = MODULE_HEADER.replace("{NAME}", name).replace("{ID}", &id);
        let source = MODULE_SOURCE.replace("{NAME}", name);
        std::fs::write(src.join(format!("{name}.h")), header).map_err(|e| e.to_string())?;
        std::fs::write(src.join(format!("{name}.cpp")), source).map_err(|e| e.to_string())?;
        return Ok(());
    }

    let (header_tpl, source_tpl, export_tpl) = match kind {
        Kind::Scene => (SCENE_HEADER, SCENE_SOURCE, SCENE_EXPORT),
        Kind::Job => (JOB_HEADER, JOB_SOURCE, JOB_EXPORT),
        Kind::Module => unreachable!("handled above"),
    };

    let header = header_tpl.replace("{NAME}", name);
    let source = source_tpl.replace("{NAME}", name);
    let export = export_tpl
        .replace("{HEADER}", &format!("{name}.h"))
        .replace("{CLASS}", name);

    std::fs::write(src.join(format!("{name}.h")), header).map_err(|e| e.to_string())?;
    std::fs::write(src.join(format!("{name}.cpp")), source).map_err(|e| e.to_string())?;
    std::fs::write(src.join("export.cpp"), export).map_err(|e| e.to_string())?;
    Ok(())
}

fn write_gitignore(root: &Path) -> Result<(), String> {
    std::fs::write(root.join(".gitignore"), GITIGNORE).map_err(|e| e.to_string())
}

/// A quick, dependency-free accent color (xorshift seeded from the wall clock), biased
/// toward the bright, saturated range the old Hub used.
pub fn random_color() -> [f32; 3] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b9)
        | 1;
    let mut s = seed;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 1000) as f32 / 1000.0
    };
    [0.4 + next() * 0.5, 0.4 + next() * 0.5, 0.4 + next() * 0.5]
}

// --- Recent-projects index (per machine, paths only) ----------------------------------

#[derive(Default, Serialize, Deserialize)]
struct RecentCache {
    projects: Vec<RecentEntry>,
}

#[derive(Serialize, Deserialize)]
struct RecentEntry {
    path: String,
}

fn load_cache() -> RecentCache {
    std::fs::read_to_string(paths::recent_projects_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &RecentCache) -> Result<(), String> {
    let file = paths::recent_projects_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Recent project roots that still exist on disk, most-recent first.
pub fn recent_paths() -> Vec<PathBuf> {
    load_cache()
        .projects
        .into_iter()
        .map(|e| PathBuf::from(e.path))
        .filter(|p| p.exists())
        .collect()
}

/// Add (or move to front) a project in the recent index.
pub fn add_recent(path: &Path) -> Result<(), String> {
    let key = path.to_string_lossy().into_owned();
    let mut cache = load_cache();
    cache.projects.retain(|e| e.path != key);
    cache.projects.insert(0, RecentEntry { path: key });
    save_cache(&cache)
}

/// Drop a project from the recent index (does not touch files on disk).
pub fn remove_recent(path: &Path) -> Result<(), String> {
    let key = path.to_string_lossy().into_owned();
    let mut cache = load_cache();
    cache.projects.retain(|e| e.path != key);
    save_cache(&cache)
}

// --- Build profiles (per machine, per project) ----------------------------------------

/// The CMake configurations a project can be built in.
///
/// Fixed rather than user-defined, because each one is a CMake build type with meaning to the
/// compiler — inventing a fifth would produce a build tree with no optimisation or debug flags at
/// all. The names are spelled exactly as `CMAKE_BUILD_TYPE` takes them, since they are used
/// verbatim as the preset name, the build directory suffix and the configuration a multi-config
/// generator selects.
pub const PROFILES: &[&str] = &["Debug", "Release", "RelWithDebInfo", "MinSizeRel"];

/// The profile a project is built in unless it says otherwise.
pub const DEFAULT_PROFILE: &str = "Debug";

/// Is this one of the profiles the Hub generates presets for?
///
/// Worth checking at every entry point: the string becomes a directory name and a CMake preset
/// name, so an unknown one produces a confusing CMake failure rather than an answer.
pub fn is_profile(profile: &str) -> bool {
    PROFILES.contains(&profile)
}

#[derive(Default, Serialize, Deserialize)]
struct StateCache {
    /// Project root -> that project's machine-local state.
    #[serde(default)]
    projects: std::collections::BTreeMap<String, ProjectState>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ProjectState {
    /// Empty means "never chosen", which resolves to [`DEFAULT_PROFILE`].
    profile: String,
}

fn load_state() -> StateCache {
    std::fs::read_to_string(paths::project_state_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(cache: &StateCache) -> Result<(), String> {
    let file = paths::project_state_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Which profile this project builds in on this machine. Always a valid profile: a state file
/// carrying something the Hub no longer generates falls back to the default rather than producing
/// a preset name nothing defines.
pub fn profile(project_root: &Path) -> String {
    let key = project_root.to_string_lossy().into_owned();
    load_state()
        .projects
        .get(&key)
        .map(|s| s.profile.clone())
        .filter(|p| is_profile(p))
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string())
}

/// Remember the profile to build this project in.
pub fn set_profile(project_root: &Path, profile: &str) -> Result<(), String> {
    if !is_profile(profile) {
        return Err(format!(
            "'{profile}' is not a build profile — expected one of {}",
            PROFILES.join(", ")
        ));
    }
    let key = project_root.to_string_lossy().into_owned();
    let mut cache = load_state();
    cache.projects.entry(key).or_default().profile = profile.to_string();
    save_state(&cache)
}

/// Remove a project from the recent list, optionally deleting its folder from disk.
///
/// Refuses any directory that holds no `koral.json`. That guard is the whole safety story: this is
/// a recursive delete driven by a path that round-tripped through the UI, and the one thing it must
/// never do is empty a directory that is not a Koral project.
///
/// Files go first — dropping the list entry and *then* failing to delete would strand a folder the
/// Hub no longer shows, which is the worst of both outcomes.
pub fn delete(project_root: &Path, delete_files: bool) -> Result<(), String> {
    if delete_files {
        if !project_root.join(CONFIG_FILE).is_file() {
            return Err(format!(
                "refusing to delete {}: it has no {CONFIG_FILE}, so it is not a Koral project",
                project_root.display()
            ));
        }
        std::fs::remove_dir_all(project_root)
            .map_err(|e| format!("failed to delete {}: {e}", project_root.display()))?;
    }

    // Drop the machine-local state too, so a later project created at the same path does not
    // inherit a build profile chosen for something else.
    let key = project_root.to_string_lossy().into_owned();
    let mut state = load_state();
    if state.projects.remove(&key).is_some() {
        let _ = save_state(&state);
    }

    remove_recent(project_root)
}

// --- Source templates -----------------------------------------------------------------
// Namespaced `kor::` for the renamed API. These are intentionally minimal; build
// scaffolding (CMakeLists/presets) is generated later against the resolved SDK.
//
// Both kinds include the umbrella <koral.h>, which pulls in the whole public API. Individual
// headers still work if a translation unit wants to stay lean, but a starting template should
// not make you go hunting for which header a type lives in.

const SCENE_HEADER: &str = r#"#pragma once

#include <koral.h>

class {NAME} final : public kor::Scene
{
public:
    void Initialize() override;
    void Update() override;
    void Render(kor::CommandBuffer& commandBuffer) override;
    void RenderUI() override;
};
"#;

const SCENE_SOURCE: &str = r#"#include "{NAME}.h"

void {NAME}::Initialize()
{
    // TODO: set up resources
}

void {NAME}::Update()
{
    // TODO: per-frame logic
}

void {NAME}::Render(kor::CommandBuffer& commandBuffer)
{
    commandBuffer
        .BeginRendering()
        .EndRendering();
}

void {NAME}::RenderUI()
{
    // TODO: define the scene UI
}
"#;

const SCENE_EXPORT: &str = r#"#include "{HEADER}"

#if defined(_WIN32)
    #define KORAL_EXPORT extern "C" __declspec(dllexport)
#else
    #define KORAL_EXPORT extern "C" __attribute__((visibility("default")))
#endif

// The engine runs the windowed path because this library exports CreateScene.
KORAL_EXPORT kor::Scene* CreateScene()
{
    return new {CLASS}();
}
"#;

const JOB_HEADER: &str = r#"#pragma once

#include <koral.h>

class {NAME} final : public kor::Job
{
public:
    kor::Task<void> Run() override;
};
"#;

const JOB_SOURCE: &str = r#"#include "{NAME}.h"

// Runs once on a headless device context — no window, surface or swap chain — and the program
// exits when this returns. Because it is a kor::Task, it may co_await background work; the
// engine drives it to completion before tearing the device down.
kor::Task<void> {NAME}::Run()
{
    kor::log::info("{NAME} running");

    // TODO: do the work (offscreen render, compute, asset processing…)

    co_return;
}
"#;

// The module templates split along the boundary that matters: the header is what *other projects*
// include (pure-virtual interface, plain structs, nothing else), and the .cpp is what only this
// module compiles. A consumer never links the module — it names it in koral.json and calls
// kor::useModule<{NAME}>() at runtime.

const MODULE_HEADER: &str = r#"#pragma once

// {NAME}'s public interface: the only file consumers see. Keep it to pure-virtual methods and
// plain structs — the implementation lives in the module and arrives at runtime, when a project
// lists "{ID}" under "modules" in its koral.json and calls:
//
//     auto* {ID} = kor::useModule<{NAME}>();

#include <koral.h>

class {NAME} : public kor::Module
{
public:
    static constexpr std::string_view kModuleId      = "{ID}";
    static constexpr std::uint32_t    kModuleVersion = 1;   // bump on any change to this interface

    // TODO: the interface consumers call, e.g.
    // virtual kor::Resource<Thing> create(const Thing::Desc& desc) = 0;
    virtual int answer() const = 0;
};
"#;

const MODULE_SOURCE: &str = r#"#include "{NAME}.h"

// The implementation. Nothing here is visible to consumers, so it can change freely between
// releases — only the header is a contract.
class {NAME}Impl final : public {NAME}
{
public:
    int answer() const override { return 42; }

private:
    // The runtime-facing lifecycle, private on purpose: the runtime reaches these virtually
    // through kor::Module*, while a consumer holding the {NAME} interface cannot call them.
    void Initialize() override
    {
        // Every module is loaded and findable here; look up dependencies with kor::useModule<T>().
    }

    void Update() override
    {
        // Once per frame, before the scene's Update.
    }

    void Shutdown() override
    {
        // Dependencies are still alive here; modules shut down in reverse dependency order.
    }
};

// The two symbols that make this library a module: a descriptor the loader reads before
// constructing anything, and the factory. Use KORAL_DECLARE_MODULE_DEPS to declare dependencies
// on other modules.
KORAL_DECLARE_MODULE({NAME}Impl)
"#;

const JOB_EXPORT: &str = r#"#include "{HEADER}"

#if defined(_WIN32)
    #define KORAL_EXPORT extern "C" __declspec(dllexport)
#else
    #define KORAL_EXPORT extern "C" __attribute__((visibility("default")))
#endif

// The engine runs the headless path because this library exports CreateJob rather than
// CreateScene. A project exports exactly one of the two.
KORAL_EXPORT kor::Job* CreateJob()
{
    return new {CLASS}();
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn scratch() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("koral-project-test-{n}"))
    }

    /// End-to-end of the git story: a created project is a committed repo, and cloning it (which is
    /// exactly what import does) yields a folder that loads back as the same project.
    #[test]
    fn create_git_inits_and_the_clone_reloads_as_a_project() {
        let base = scratch();
        std::fs::create_dir_all(&base).unwrap();

        let root = create(&base, "MyProj", "0.0.1", [0.5, 0.5, 0.5], Kind::Scene).unwrap();
        assert!(root.join(".git").is_dir(), "create should git-init the project");
        assert!(
            crate::git::info(&root).and_then(|g| g.branch).is_some(),
            "the initial commit gives HEAD a branch"
        );

        // Clone it locally, the way import_project clones a remote, and confirm it's a valid project.
        let dest = base.join("cloned");
        crate::git::clone(root.to_str().unwrap(), &dest).unwrap();
        assert_eq!(load(&dest).unwrap().name, "MyProj");

        std::fs::remove_dir_all(&base).ok();
    }

    /// Everything the Hub generates is ignored, because every one of them is derived from
    /// `koral.json` and rewritten on the next build — a committed copy can only ever be stale.
    /// The project's own sources and `koral.json` are what a clone needs, and must stay tracked.
    #[test]
    fn generated_build_files_are_ignored_but_the_project_is_not() {
        let base = scratch();
        std::fs::create_dir_all(&base).unwrap();
        let root = create(&base, "Ignored", "0.0.9", [0.5, 0.5, 0.5], Kind::Scene).unwrap();

        let rules: Vec<String> = std::fs::read_to_string(root.join(".gitignore"))
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .collect();

        for generated in [
            "/CMakeLists.txt",
            "/CMakePresets.json",
            "/vcpkg.json",
            "/.idea/",
            "/.vscode/launch.json",
            "/.vscode/c_cpp_properties.json",
            "/.koral/",
            "/cmake-build-*/",
        ] {
            assert!(rules.iter().any(|r| r == generated), "{generated} should be ignored");
        }

        // The line that matters in the other direction: ignoring the sources, or the config the
        // whole scheme derives from, would make a shared project unbuildable.
        for kept in ["koral.json", "/src/", "src", ".gitignore"] {
            assert!(!rules.iter().any(|r| r == kept), "{kept} must NOT be ignored");
        }

        std::fs::remove_dir_all(&base).ok();
    }

    /// A module scaffold is two files — the interface header and the implementation — with the
    /// module id derived from the name, and no export.cpp: KORAL_DECLARE_MODULE *is* the export.
    #[test]
    fn create_module_scaffolds_interface_and_impl() {
        let base = scratch();
        std::fs::create_dir_all(&base).unwrap();

        let root = create(&base, "MyCameras", "0.0.9", [0.5, 0.5, 0.5], Kind::Module).unwrap();
        assert_eq!(load(&root).unwrap().kind, Kind::Module);

        let header = std::fs::read_to_string(root.join("src/MyCameras.h")).unwrap();
        assert!(header.contains("kModuleId      = \"mycameras\""), "{header}");
        assert!(header.contains("public kor::Module"), "{header}");

        let source = std::fs::read_to_string(root.join("src/MyCameras.cpp")).unwrap();
        assert!(source.contains("KORAL_DECLARE_MODULE(MyCamerasImpl)"), "{source}");

        assert!(
            !root.join("src/export.cpp").exists(),
            "a module's entry points come from KORAL_DECLARE_MODULE, not an export.cpp"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}

const GITIGNORE: &str = r#"# Build output
/build/
/cmake-build-*/

# Generated by Koral Hub from koral.json, on every build and every "Open in IDE". Editing one is
# pointless — the next build overwrites it — and committing one just puts a copy of what
# koral.json already says into the history, where it goes stale. koral.json is the source of
# truth; these are derived from it. CMakePresets.json additionally holds this machine's absolute
# SDK paths, so it could never be shared anyway.
/CMakeLists.txt
/CMakePresets.json
/vcpkg.json

# Hub-generated IDE state (absolute paths — regenerated on each build).
# .vscode/tasks.json, settings.json and extensions.json carry no absolute paths and *are*
# committed — though they still need the Hub (or a manual cmake configure) to produce
# CMakePresets.json before they can run anything.
/.idea/
/.vscode/launch.json
/.vscode/c_cpp_properties.json

# Hub-managed local state
/.koral/
"#;

