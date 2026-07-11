mod commands;
mod framework;
mod model;
mod paths;
mod project;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::list_recent_projects,
            commands::create_project,
            commands::remove_recent,
            commands::default_project_location,
            commands::installed_frameworks,
            commands::ensure_framework,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Koral Hub");
}
