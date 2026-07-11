//! Project storage: read/write a project's portable `koral.json`, scaffold a new project,
//! and maintain the per-machine recent-projects index.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::ProjectConfig;
use crate::paths;

/// Committed, portable project metadata file.
pub const CONFIG_FILE: &str = "koral.json";

// --- Load / save the portable config --------------------------------------------------

pub fn load(project_root: &Path) -> Result<ProjectConfig, String> {
    let file = project_root.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", file.display()))
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

    save(&root, &ProjectConfig::new(name, framework_version, color))?;
    write_sources(&root, name)?;
    write_gitignore(&root)?;

    Ok(root)
}

fn write_sources(root: &Path, name: &str) -> Result<(), String> {
    let src = root.join("src");
    let header = TEMPLATE_HEADER.replace("{NAME}", name);
    let source = TEMPLATE_SOURCE.replace("{NAME}", name);
    let export = TEMPLATE_EXPORT
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

// --- Source templates -----------------------------------------------------------------
// Namespaced `kor::` for the renamed API. These are intentionally minimal; build
// scaffolding (CMakeLists/presets) is generated later against the resolved SDK.

const TEMPLATE_HEADER: &str = r#"#pragma once

#include <scene.h>

class {NAME} final : public kor::Scene
{
public:
    void Initialize() override;
    void Update() override;
    void Render(kor::CommandBuffer& commandBuffer) override;
    void RenderUI(ImGuiContext* context) override;
};
"#;

const TEMPLATE_SOURCE: &str = r#"#include "{NAME}.h"

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

void {NAME}::RenderUI(ImGuiContext* context)
{
    // TODO: define the scene UI
}
"#;

const TEMPLATE_EXPORT: &str = r#"#include "{HEADER}"

#if defined(_WIN32)
    #define SCENE_EXPORT extern "C" __declspec(dllexport)
#else
    #define SCENE_EXPORT extern "C" __attribute__((visibility("default")))
#endif

SCENE_EXPORT kor::Scene* CreateScene()
{
    return new {CLASS}();
}
"#;

const GITIGNORE: &str = r#"# Build output
/build/
/cmake-build-*/

# Hub-managed local state
/.koral/
"#;
