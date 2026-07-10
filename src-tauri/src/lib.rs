mod commands;
mod model;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::list_recent_projects,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Koral Hub");
}
