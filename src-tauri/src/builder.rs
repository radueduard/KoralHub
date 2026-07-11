//! Build + run orchestration.
//!
//! Given a project folder, this resolves (installing if needed) the framework version it
//! declares, regenerates the build scaffolding against that SDK, drives CMake to configure
//! and build, and launches the SDK's runtime on the resulting scene library. Output is
//! streamed to the UI as `build-output` events; completion as `build-finished`.

use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::{AppHandle, Emitter};

use crate::{framework, project, scaffold};

/// Platform-specific shared-library file name for a scene target.
pub fn lib_file_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

struct BuildOutcome {
    sdk_root: PathBuf,
    runtime_rel: String,
    lib_path: PathBuf,
}

/// Configure + build the project, streaming output to the UI. Returns where the built
/// scene library should be.
fn build(app: &AppHandle, project_root: &Path, profile: &str) -> Result<BuildOutcome, String> {
    let cfg = project::load(project_root)?;

    emit(app, &format!("Resolving koral {}…\n", cfg.framework_version));
    let sdk_root = framework::ensure_installed(&cfg.framework_version)?;
    let manifest = framework::read_manifest(&sdk_root)?;

    emit(app, "Generating build files…\n");
    scaffold::generate(project_root, &cfg, &sdk_root, &manifest, profile)?;

    let configure = || {
        let mut c = Command::new("cmake");
        c.arg("--preset").arg(profile).current_dir(project_root);
        c
    };
    emit(app, &format!("$ cmake --preset {profile}\n"));

    if let Err(e) = run_step(app, &mut configure()) {
        // A CMakeCache.txt pins the toolchain, compiler and SDK paths it was first configured
        // with, and keeps honouring them even after the preset stops setting them. So a cache
        // left behind by a stale SDK — or by a preset we have since fixed — fails identically
        // forever, and regenerating the scaffolding cannot dislodge it. The Hub owns this
        // directory, so the safe move is to throw it away and configure once more.
        let build_dir = project_root.join(scaffold::build_dir_name(profile));
        if !build_dir.exists() {
            return Err(e);
        }
        emit(app, "\nConfigure failed — clearing the build directory and retrying…\n");
        std::fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("failed to clear {}: {e}", build_dir.display()))?;
        emit(app, &format!("$ cmake --preset {profile}\n"));
        run_step(app, &mut configure())?;
    }

    let mut compile = Command::new("cmake");
    compile
        .arg("--build")
        .arg("--preset")
        .arg(profile)
        .current_dir(project_root);
    emit(app, &format!("$ cmake --build --preset {profile}\n"));
    run_step(app, &mut compile)?;

    let lib_path = project_root
        .join(scaffold::build_dir_name(profile))
        .join(lib_file_name(&cfg.name));

    Ok(BuildOutcome {
        sdk_root,
        runtime_rel: manifest.runtime,
        lib_path,
    })
}

/// Build the project (as a `build-*` event stream) and return once done.
pub fn build_only(app: &AppHandle, project_root: &Path, profile: &str) -> Result<(), String> {
    build(app, project_root, profile).map(|_| ())
}

/// Build the project, then launch the SDK runtime on its scene library.
pub fn run(app: &AppHandle, project_root: &Path, profile: &str) -> Result<(), String> {
    let outcome = build(app, project_root, profile)?;

    if !outcome.lib_path.exists() {
        return Err(format!(
            "built library not found: {}",
            outcome.lib_path.display()
        ));
    }

    let runtime = outcome.sdk_root.join(&outcome.runtime_rel);
    let args = runtime_args(&outcome.lib_path);

    emit(
        app,
        &format!("$ {} {}\n", runtime.display(), args.join(" ")),
    );

    Command::new(&runtime)
        .args(&args)
        .spawn()
        .map_err(|e| format!("failed to launch runtime {}: {e}", runtime.display()))?;
    Ok(())
}

/// The runtime invocation: the scene library, and nothing else.
///
/// Every run setting — the API, the window, the asset and shader directories — lives in the
/// project's `koral.json`, and the runtime reads that file itself: it walks up from the scene
/// library it is given until it finds one, which lands on the project root the library was built
/// under. So there is nothing to pass, and nothing that can disagree.
///
/// This is deliberately the *whole* launch, and it is why `scaffold` can hand the IDEs the same
/// bare command: the settings cannot drift between the Hub's ▶ and a Run from CLion, because
/// neither of them carries the settings. Flags still exist on the runtime (`--width`, `--api`, …)
/// and still override the file — they are for a one-off run, not for wiring a project up.
///
/// Shared with `scaffold`.
pub fn runtime_args(lib: &Path) -> Vec<String> {
    vec![lib.to_string_lossy().into_owned()]
}

/// Run one child process to completion, forwarding its stdout+stderr to the UI. Errors if
/// the process can't launch or exits non-zero.
fn run_step(app: &AppHandle, cmd: &mut Command) -> Result<(), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to launch {:?}: {e}", cmd.get_program()))?;

    if !output.stdout.is_empty() {
        emit(app, &String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        emit(app, &String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        return Err(format!("command failed ({})", output.status));
    }
    Ok(())
}

fn emit(app: &AppHandle, text: &str) {
    let _ = app.emit("build-output", text.to_string());
}
