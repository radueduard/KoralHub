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

/// Scaffold the sources for this kind of app.
///
/// The library exports exactly one entry point — `CreateScene` or `CreateJob` — and the engine
/// decides which path to run by which symbol it finds. Exporting the wrong one for the kind would
/// simply never be picked up.
fn write_sources(root: &Path, name: &str, kind: Kind) -> Result<(), String> {
    let src = root.join("src");
    let (header_tpl, source_tpl, export_tpl) = match kind {
        Kind::Scene => (SCENE_HEADER, SCENE_SOURCE, SCENE_EXPORT),
        Kind::Job => (JOB_HEADER, JOB_SOURCE, JOB_EXPORT),
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
}

const GITIGNORE: &str = r#"# Build output
/build/
/cmake-build-*/

# Machine-specific, regenerated by Koral Hub each build (absolute SDK/vcpkg paths)
/CMakePresets.json

# Hub-generated IDE state (absolute paths — regenerated on each build).
# .vscode/tasks.json and .vscode/extensions.json are portable and *are* committed, so that a
# fresh clone can build and run from the IDE without opening the Hub first.
/.idea/
/.vscode/launch.json

# Hub-managed local state
/.koral/
"#;

