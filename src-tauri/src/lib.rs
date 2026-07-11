mod builder;
mod commands;
#[cfg(debug_assertions)]
mod dev_server;
mod framework;
mod git;
mod ide;
mod model;
mod paths;
mod project;
mod scaffold;
mod settings;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK's DMABUF renderer crashes on a number of Wayland setups (NVIDIA, and
    // several compositors) with "Error 71 (Protocol error) dispatching to Wayland display",
    // killing the app before the window appears. Fall back to the plain renderer. Must be
    // set before the webview initializes, so it lives at the very top of run().
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // Debug builds load the Vite dev server (build.devUrl). When launched from an IDE's Run/Debug
    // button (a plain `cargo run`) nothing has started it, so start it ourselves and hold the
    // guard for the whole run so it's torn down on exit. `tauri dev` already starts Vite, so this
    // is a no-op there. Release builds serve the bundled frontend and skip this entirely.
    #[cfg(debug_assertions)]
    let _dev_server = dev_server::ensure();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::list_recent_projects,
            commands::create_project,
            commands::import_project,
            commands::remove_project,
            commands::default_project_location,
            commands::settings,
            commands::save_settings,
            commands::resolved_defaults,
            commands::installed_ides,
            commands::open_in_ide,
            commands::project_config,
            commands::save_project_config,
            commands::installed_frameworks,
            commands::available_frameworks,
            commands::install_framework,
            commands::uninstall_framework,
            commands::ensure_framework,
            commands::build_project,
            commands::run_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Koral Hub");
}
