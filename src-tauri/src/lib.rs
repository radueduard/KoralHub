mod auth;
mod builder;
mod collection;
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

/// Recover a usable `PATH` when launched from a GUI.
///
/// A `.app` opened from Finder/Dock — or an IDE that was itself opened that way, whose Run button
/// then spawns us — inherits macOS's bare launchd `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`). That
/// omits Homebrew, so `cmake`, the `ninja` it drives and the compiler all fail to launch with
/// "No such file or directory", even though a terminal finds them fine. Rebuild `PATH` from the
/// user's login shell (which sources their profile) plus a few well-known toolchain dirs as a
/// backstop, and set it on our own process so every tool we spawn inherits it.
///
/// Unix-only; a Windows GUI process already gets the full system `PATH`.
#[cfg(unix)]
fn repair_path() {
    use std::collections::HashSet;

    // A login shell sources .zprofile/.zshenv (.bash_profile for bash), where Homebrew, rustup and
    // friends put themselves on PATH. Non-interactive (`-lc`, not `-lic`) so a heavyweight .zshrc
    // prompt can't hang startup; the backstop below covers anything only .zshrc would have added.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let from_shell = std::process::Command::new(&shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    // Common toolchain locations, in case the shell probe failed or missed one. Homebrew first so
    // cmake/ninja resolve here even when the profile never ran.
    let extras = "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin";
    let current = std::env::var("PATH").unwrap_or_default();

    // Merge in priority order (what we have, the shell's PATH, then the backstop), deduping while
    // preserving order.
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for source in [current.as_str(), from_shell.as_str(), extras] {
        for dir in source.split(':').filter(|s| !s.is_empty()) {
            if seen.insert(dir.to_owned()) {
                dirs.push(dir);
            }
        }
    }

    if let Ok(joined) = std::env::join_paths(&dirs) {
        std::env::set_var("PATH", joined);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Two WebKitGTK GPU paths have to be defused before the webview initializes, so both live at
    // the very top of run(). Each is skipped when already set, so a machine that does better on
    // the accelerated path can opt back in.
    //
    //  - DMABUF renderer: crashes on a number of Wayland setups (NVIDIA, and several compositors)
    //    with "Error 71 (Protocol error) dispatching to Wayland display", killing the app before
    //    the window appears.
    //  - Accelerated compositing: when WebKit cannot get an EGL context it does not fall back to
    //    software, it just paints nothing — the window opens blank/white and stays that way. This
    //    is what the v0.1.0 AppImage hit: the bundle carries its own webkit/GL stack, which fails
    //    to initialize EGL against a host Mesa it wasn't built against. Disabling compositing
    //    routes rendering through the software path. The Hub's UI is plain DOM, so that costs
    //    nothing noticeable.
    #[cfg(target_os = "linux")]
    for (key, value) in [
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
        ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
    ] {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }

    // Must run before we spawn any external tool (cmake, ninja, the SDK runtime, an IDE).
    #[cfg(unix)]
    repair_path();

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
            commands::list_collections,
            commands::add_collection,
            commands::remove_collection,
            commands::download_lab,
            commands::list_authored_collections,
            commands::create_collection,
            commands::add_lab_to_collection,
            commands::add_project_to_collection,
            commands::remove_lab_from_collection,
            commands::reorder_lab_in_collection,
            commands::remove_authored_collection,
            commands::device_login_start,
            commands::open_url,
            commands::list_accounts,
            commands::sign_out,
            commands::publish_collection,
            commands::publish_project,
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
