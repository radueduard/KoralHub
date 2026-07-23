//! Build + run orchestration.
//!
//! Given a project folder, this resolves (installing if needed) the framework version it
//! declares, regenerates the build scaffolding against that SDK, drives CMake to configure
//! and build, and launches the SDK's runtime on the resulting scene library. Output is
//! streamed to the UI as `build-output` events; completion as `build-finished`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tauri::{AppHandle, Emitter};

use crate::{framework, project, scaffold};

/// Build a `Command` for an external tool with any inherited loader environment stripped out.
///
/// When the Hub runs as an AppImage — or is launched from something that does the same — its own
/// process has `LD_LIBRARY_PATH`/`LD_PRELOAD` pointing at *bundled* libraries. A tool we spawn
/// (cmake, ninja and the compiler it drives, the SDK runtime) would inherit that and load those
/// bundled libraries against the system ones, which fails with symbol-lookup errors — classically
/// the system `libcurl` pairing with an older bundled `libnghttp2`. These tools must use the
/// system libraries, so we drop the two variables for the child only; the Hub's own environment
/// is left untouched. No-op on non-Linux, where there is nothing to strip.
pub(crate) fn external_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "linux")]
    {
        cmd.env_remove("LD_LIBRARY_PATH");
        cmd.env_remove("LD_PRELOAD");
    }
    #[cfg(target_os = "windows")]
    {
        // Every console child (cmake, ninja, the compiler, the runtime) would otherwise flash its
        // own black cmd window — which looks alarming and clutters the screen during a build. The
        // CREATE_NO_WINDOW flag (0x08000000) runs them with no console; their output is piped to the
        // Hub either way, so nothing is lost. A child's own GUI window (the app) still appears.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    // We pipe every child's output, so tools see stdout is not a TTY and (cmake, ninja, gcc/clang,
    // and many apps) strip ANSI colour by default. Ask them to emit it anyway, so the console reads
    // like a real terminal. Each variable is honoured by a different tool family; unknown ones are
    // ignored, so setting them all is safe.
    cmd.env("CLICOLOR_FORCE", "1"); // cmake, ninja, BSD-style tools
    cmd.env("FORCE_COLOR", "1"); // Node-ecosystem tools, many apps
    cmd.env("CMAKE_COLOR_DIAGNOSTICS", "ON"); // makes CMake pass -fdiagnostics-color to the compiler
    cmd
}

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
        let mut c = external_command("cmake");
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

    let mut compile = external_command("cmake");
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

    // The launch line and everything the app prints belong on the Output tab, not the Build tab.
    emit_run(
        app,
        &format!("$ {} {}\n", runtime.display(), args.join(" ")),
    );

    // Launch the app under a pseudo-terminal. Attached to a PTY it sees a real, colour-capable TTY
    // and so emits ANSI colour exactly as it would in a terminal — which piping its stdout could
    // never achieve, since programs disable colour when their output is not a terminal. On Windows
    // the PTY is a ConPTY, which also means no extra console window pops up.
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 40, cols: 140, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("failed to open a pseudo-terminal: {e}"))?;

    let mut cmd = CommandBuilder::new(&runtime);
    cmd.args(&args);
    cmd.env("TERM", "xterm-256color");
    // Match `external_command`: don't hand the app the Hub's bundled-library loader environment.
    // The Linux windowing backend is not set here — it rides in as the runtime's `--platform` flag
    // (see `runtime_args`), and so is already visible in the launch line printed above.
    #[cfg(target_os = "linux")]
    {
        cmd.env_remove("LD_LIBRARY_PATH");
        cmd.env_remove("LD_PRELOAD");
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("failed to launch runtime {}: {e}", runtime.display()))?;
    // Drop our handle to the slave so the reader below sees EOF once the app (the last slave holder)
    // exits, rather than blocking forever.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to read the app's output: {e}"))?;
    let app_out = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = app_out.emit("run-output", String::from_utf8_lossy(&buf[..n]).into_owned());
                }
            }
        }
    });

    // Wait for the app in the background, holding the master open for its whole lifetime (dropping it
    // early would SIGHUP the app), then note the exit code on the Output tab.
    let app_wait = app.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        drop(pair.master);
        if let Ok(status) = status {
            let _ = app_wait.emit("run-output", format!("\n[app exited: {}]\n", status.exit_code()));
        }
    });
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
/// The one exception is the Linux windowing backend: which windowing system the app opens on is a
/// per-machine fact, so it cannot live in the portable `koral.json` (which defaults `platform` to
/// `auto`). When the user has pinned one, the Hub rides it in as the runtime's own `--platform`
/// flag — not by setting SDL/Qt/GTK environment variables, which this GLFW-based runtime never read.
/// Emitting it here rather than only on the Hub's ▶ is deliberate: it keeps a Run from CLion or
/// VS Code opening on the same backend, since they launch this exact command.
///
/// Shared with `scaffold`.
pub fn runtime_args(lib: &Path) -> Vec<String> {
    let mut args = vec![lib.to_string_lossy().into_owned()];
    #[cfg(target_os = "linux")]
    {
        // `""` means "no preference" (leave `platform` at the config's `auto`); `"x11"` / `"wayland"`
        // are exactly the runtime's `--platform` values, so no translation is needed.
        let backend = crate::settings::load().display_backend;
        let backend = backend.trim();
        if !backend.is_empty() {
            args.push("--platform".into());
            args.push(backend.to_string());
        }
    }
    args
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

/// Build-tab output: configure/compile progress and diagnostics.
fn emit(app: &AppHandle, text: &str) {
    let _ = app.emit("build-output", text.to_string());
}

/// Output-tab output: the launch line and the running app's own stdout/stderr.
fn emit_run(app: &AppHandle, text: &str) {
    let _ = app.emit("run-output", text.to_string());
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The whole point of `external_command`: a child must not inherit the AppImage's
    /// `LD_LIBRARY_PATH`, which is what made cmake load bundled libraries and crash.
    #[test]
    fn external_command_strips_the_loader_environment() {
        std::env::set_var("LD_LIBRARY_PATH", "/appimage/bundled/lib");
        std::env::set_var("LD_PRELOAD", "/appimage/bundled/preload.so");

        let out = external_command("sh")
            .args(["-c", "printf '%s|%s' \"${LD_LIBRARY_PATH-unset}\" \"${LD_PRELOAD-unset}\""])
            .output()
            .expect("sh should run");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|unset");

        std::env::remove_var("LD_LIBRARY_PATH");
        std::env::remove_var("LD_PRELOAD");
    }

    /// The premise of running the app under a PTY: its stdout is a real terminal, which is what makes
    /// programs emit ANSI colour. If this ever regressed, the Output tab would go back to plain text.
    #[test]
    fn a_pty_child_sees_a_tty() {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "test -t 1 && printf TTY || printf PIPE"]);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        child.wait().ok();
        let mut out = String::new();
        reader.read_to_string(&mut out).ok();
        assert!(out.contains("TTY"), "the child's stdout should be a tty, got: {out:?}");
    }
}
