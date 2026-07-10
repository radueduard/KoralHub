use serde::Serialize;

/// A recent-projects list item sent to the UI.
///
/// `path` is machine-local (read from the Hub's per-machine recent-projects cache); the
/// remaining fields come from each project's committed, portable Koral config. Serialized
/// as camelCase so the field names match the TypeScript `RecentProject` type.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    pub name: String,
    pub path: String,
    /// Accent color, linear RGB in [0, 1].
    pub color: [f32; 3],
    /// Koral framework release the project builds against.
    pub framework_version: String,
}

/// Returns the projects to show on the home screen.
///
/// TODO: load the per-machine recent-projects cache, then read each project's portable
/// Koral config (see `model::ProjectConfig`). For now this returns sample data so the UI
/// has something to render during bring-up.
#[tauri::command]
pub fn list_recent_projects() -> Vec<RecentProject> {
    vec![
        RecentProject {
            name: "Sample".into(),
            path: "/home/radue/koral/Sample".into(),
            color: [0.36, 0.55, 0.94],
            framework_version: "0.0.1".into(),
        },
        RecentProject {
            name: "Particles".into(),
            path: "/home/radue/koral/Particles".into(),
            color: [0.85, 0.45, 0.35],
            framework_version: "0.0.1".into(),
        },
    ]
}
