//! Thin `#[tauri::command]` layer exposed to the SolidJS frontend. Real work lives in the
//! `project` and `framework` modules; these adapt to/from UI-shaped DTOs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::framework::{self, InstalledFramework};
use crate::paths;
use crate::project;

/// A recent-projects list item sent to the UI. `path` is machine-local (from the recent
/// index); the rest is read from each project's committed, portable Koral config.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    pub name: String,
    pub path: String,
    /// Accent color, linear RGB in [0, 1].
    pub color: [f32; 3],
    pub framework_version: String,
}

/// The projects to show on the home screen: recent paths whose `koral.json` still loads.
#[tauri::command]
pub fn list_recent_projects() -> Vec<RecentProject> {
    project::recent_paths()
        .into_iter()
        .filter_map(|path| {
            let cfg = project::load(&path).ok()?;
            Some(RecentProject {
                name: cfg.name,
                path: path.to_string_lossy().into_owned(),
                color: cfg.color,
                framework_version: cfg.framework_version,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectRequest {
    /// Parent folder; the project is created in `location/name`.
    pub location: String,
    pub name: String,
    /// Framework release to target; defaults to the latest known if omitted.
    #[serde(default)]
    pub framework_version: Option<String>,
}

/// Scaffold a new project, add it to the recent index, and return its list entry.
#[tauri::command]
pub fn create_project(req: CreateProjectRequest) -> Result<RecentProject, String> {
    let version = req
        .framework_version
        .unwrap_or_else(|| DEFAULT_FRAMEWORK_VERSION.to_string());
    let color = project::random_color();

    let root = project::create(Path::new(&req.location), &req.name, &version, color)?;
    project::add_recent(&root)?;

    Ok(RecentProject {
        name: req.name,
        path: root.to_string_lossy().into_owned(),
        color,
        framework_version: version,
    })
}

/// Remove a project from the recent list (leaves files on disk untouched).
#[tauri::command]
pub fn remove_recent(path: String) -> Result<(), String> {
    project::remove_recent(Path::new(&path))
}

/// Default parent directory for new projects (`~/Koral`), for the create dialog.
#[tauri::command]
pub fn default_project_location() -> String {
    paths::default_projects_dir().to_string_lossy().into_owned()
}

/// SDKs already installed on this machine.
#[tauri::command]
pub fn installed_frameworks() -> Vec<InstalledFramework> {
    framework::installed()
}

/// Ensure a framework version is installed (downloading if needed) and return its SDK root.
#[tauri::command]
pub fn ensure_framework(version: String) -> Result<String, String> {
    framework::ensure_installed(&version).map(|p| p.to_string_lossy().into_owned())
}

const DEFAULT_FRAMEWORK_VERSION: &str = "0.0.1";
