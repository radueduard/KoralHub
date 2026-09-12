//! Build + run orchestration.
//!
//! Given a project folder, this resolves (installing if needed) the framework version it
//! declares, regenerates the build scaffolding against that SDK, drives CMake to configure
//! and build, and launches the SDK's runtime on the resulting scene library. Output is
//! streamed to the UI as `build-output` events; completion as `build-finished`. Every one of
//! those events names the project it belongs to, because each project has its own console.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

// Used by the Unix launcher (and its tests); the Windows launcher deliberately uses pipes.
#[cfg(unix)]
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::{framework, modules, project, scaffold};

/// One job's console: the project the user started it on, and the handle its output goes out on.
///
/// Jobs are per project, not per app — two projects can be building at once, and each has its own
/// Build and Output tabs — so every event carries the project it belongs to. Module dependencies
/// are compiled as part of their dependent's job, and this deliberately stays on the *originating*
/// project throughout: a module's compile output belongs on the console of the project that asked
/// for it, which is the only one the user is looking at.
#[derive(Clone)]
pub struct Console {
    app: AppHandle,
    /// The project's path exactly as the UI named it. The UI keys its consoles by this string, so
    /// it must round-trip unchanged rather than being canonicalized on the way through.
    project: String,
}

/// A chunk of output, addressed to one project's console.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Line<'a> {
    project: &'a str,
    text: &'a str,
}

/// Emitted as `build-finished` when a build/run job ends.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Finished<'a> {
    project: &'a str,
    success: bool,
    error: Option<&'a str>,
}

impl Console {
    pub fn new(app: &AppHandle, project: &str) -> Self {
        Self { app: app.clone(), project: project.to_string() }
    }

    /// The project folder this job runs in.
    pub fn root(&self) -> &Path {
        Path::new(&self.project)
    }

    /// Build-tab output: configure/compile progress and diagnostics.
    fn build(&self, text: &str) {
        let _ = self.app.emit("build-output", Line { project: &self.project, text });
    }

    /// Output-tab output: the launch line and the running app's own stdout/stderr.
    fn run(&self, text: &str) {
        let _ = self.app.emit("run-output", Line { project: &self.project, text });
    }

    /// The job is over — the project's console stops showing a build in progress.
    pub fn finished(&self, result: &Result<(), String>) {
        let _ = self.app.emit(
            "build-finished",
            Finished {
                project: &self.project,
                success: result.is_ok(),
                error: result.as_ref().err().map(String::as_str),
            },
        );
    }
}

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

        // Ninja, unlike the Visual Studio generator, does not know how to find MSVC: it needs
        // cl.exe on PATH and the INCLUDE/LIB variables that go with it. A GUI app inherits none
        // of that, so the Hub supplies what a Developer Command Prompt would. Empty when a
        // compiler is already reachable, which leaves a deliberate setup alone.
        for (key, value) in crate::msvc::environment() {
            cmd.env(key, value);
        }
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

/// The CMake to drive: the Hub's own copy when it installed one, else the system's.
///
/// Falls back to the bare name so a machine with neither still produces CMake's own "not found"
/// error rather than a panic — and the toolchain wizard is what turns that into an answer.
fn cmake() -> std::ffi::OsString {
    crate::toolchain::cmake_program()
        .map(Into::into)
        .unwrap_or_else(|| "cmake".into())
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

/// Configure + build one project (or module) tree, streaming output to the UI. Returns the SDK it
/// was built against.
///
/// Split out from [`build`] because a project's module dependencies are ordinary projects and get
/// built exactly the same way — there is no second build path to keep in step.
fn compile_tree(
    console: &Console,
    project_root: &Path,
    profile: &str,
) -> Result<(PathBuf, framework::FrameworkManifest), String> {
    let cfg = project::load(project_root)?;

    console.build(&format!("Resolving koral {}…\n", cfg.framework_version));
    // resolve(), not ensure_installed(): a version the user registered from a local build must be
    // used as-is rather than downloaded, and it carries no framework.json of its own.
    let (sdk_root, manifest) = framework::resolve(&cfg.framework_version)?;
    if framework::local::find(&cfg.framework_version).is_some() {
        console.build(&format!("Using local build at {}\n", sdk_root.display()));
    }

    console.build("Generating build files…\n");
    scaffold::generate(project_root, &cfg, &sdk_root, &manifest, profile)?;

    let configure = || {
        let mut c = external_command(cmake());
        c.arg("--preset").arg(profile).current_dir(project_root);
        c
    };
    console.build(&format!("$ cmake --preset {profile}\n"));

    if let Err(e) = run_step(console, &mut configure()) {
        // A CMakeCache.txt pins the toolchain, compiler and SDK paths it was first configured
        // with, and keeps honouring them even after the preset stops setting them. So a cache
        // left behind by a stale SDK — or by a preset we have since fixed — fails identically
        // forever, and regenerating the scaffolding cannot dislodge it. The Hub owns this
        // directory, so the safe move is to throw it away and configure once more.
        let build_dir = project_root.join(scaffold::build_dir_name(profile));
        if !build_dir.exists() {
            return Err(e);
        }
        console.build("\nConfigure failed — clearing the build directory and retrying…\n");
        std::fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("failed to clear {}: {e}", build_dir.display()))?;
        console.build(&format!("$ cmake --preset {profile}\n"));
        run_step(console, &mut configure())?;
    }

    let mut compile = external_command(cmake());
    compile
        .arg("--build")
        .arg("--preset")
        .arg(profile)
        .current_dir(project_root);
    console.build(&format!("$ cmake --build --preset {profile}\n"));
    run_step(console, &mut compile)?;

    // The tree this profile was actually built against, not the registered one. They differ
    // whenever the SDK is installed once per configuration, and ▶ has to launch the runtime that
    // matches what was just compiled — a Debug scene handed to the release runtime is the same
    // CRT mismatch that scaffold's CMAKE_MSVC_RUNTIME_LIBRARY exists to prevent, arriving by a
    // different route. scaffold::generate is still given the registered root: it resolves a tree
    // per profile itself, because the presets it writes cover all of them.
    Ok((framework::tree_for_profile(&sdk_root, profile), manifest))
}

/// Build the project and everything it needs to run: its own library, plus each module it lists
/// that is a module project registered on this machine.
///
/// A module project's library is built into *its* build tree, which the runtime never searches. So
/// after building, each one is copied in beside the scene library — a directory the runtime does
/// search — which is what makes `"modules": ["MyCameras"]` resolve for the Hub's ▶ and, because the
/// staged copies stay there, for a subsequent Run from an IDE too. Modules that come from the SDK,
/// or that are written as paths, are left alone: the runtime finds those itself.
fn build(console: &Console, profile: &str) -> Result<BuildOutcome, String> {
    let project_root = console.root();
    let cfg = project::load(project_root)?;

    // Which of this project's module entries are projects we can build. Resolved before anything
    // is compiled, so a typo'd module name is reported as "no such module" rather than after a
    // full build of everything else.
    let module_roots: Vec<(String, PathBuf)> = cfg
        .modules
        .iter()
        .filter_map(|name| modules::find_project_module(name).map(|root| (name.clone(), root)))
        .collect();

    for (name, root) in &module_roots {
        console.build(&format!("\n=== Building module {name} ===\n"));
        compile_tree(console, root, profile)?;
    }
    if !module_roots.is_empty() {
        console.build(&format!("\n=== Building {} ===\n", cfg.name));
    }

    let (sdk_root, manifest) = compile_tree(console, project_root, profile)?;

    let build_dir = project_root.join(scaffold::build_dir_name(profile));
    if !module_roots.is_empty() {
        console.build("Staging modules beside the scene library…\n");
        modules::stage(&cfg.modules, &module_roots, &build_dir, profile)?;
    }

    Ok(BuildOutcome {
        sdk_root,
        runtime_rel: manifest.runtime,
        lib_path: build_dir.join(lib_file_name(&cfg.name)),
    })
}

/// Build the project (as a `build-*` event stream) and return once done.
pub fn build_only(console: &Console, profile: &str) -> Result<(), String> {
    build(console, profile).map(|_| ())
}

/// Build the project, then launch the SDK runtime on its scene library.
pub fn run(console: &Console, profile: &str) -> Result<(), String> {
    // A module has no app to start — the runtime would load it, find neither CreateScene nor
    // CreateJob, and fail with a much less helpful message than this one. Checked before the
    // build so the user is told immediately, not after a full compile.
    let cfg = project::load(console.root())?;
    if !cfg.kind.is_runnable() {
        return Err(format!(
            "'{}' is a module — it cannot run on its own. Build it here, then run a project that \
             lists \"{}\" under \"modules\" in its koral.json.",
            cfg.name,
            cfg.name.to_lowercase()
        ));
    }

    let outcome = build(console, profile)?;

    if !outcome.lib_path.exists() {
        return Err(format!(
            "built library not found: {}",
            outcome.lib_path.display()
        ));
    }

    let runtime = outcome.sdk_root.join(&outcome.runtime_rel);
    let args = runtime_args(&outcome.lib_path);

    // The launch line and everything the app prints belong on the Output tab, not the Build tab.
    console.run(&format!("$ {} {}\n", runtime.display(), args.join(" ")));

    // Hand off to the platform launcher, which streams the app's output to the Output tab and
    // returns as soon as it is running — the app outlives this job.
    launch(console, &runtime, &args)
}

/// Forward everything a launched app writes to its project's Output tab, until it closes the stream.
///
/// Carries its own [`Console`] rather than borrowing one: the app outlives the job that started it,
/// so its output must keep landing on that project's tab whichever project the user has selected by
/// then.
fn pump(mut stream: impl Read, console: Console) {
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => console.run(&String::from_utf8_lossy(&buf[..n])),
        }
    }
}

/// Unix: launch the app under a pseudo-terminal.
///
/// Attached to a PTY it sees a real, colour-capable TTY and so emits ANSI colour exactly as it
/// would in a terminal — which piping its stdout could never achieve, since programs disable colour
/// when their output is not a terminal.
#[cfg(unix)]
fn launch(console: &Console, runtime: &Path, args: &[String]) -> Result<(), String> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 40, cols: 140, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("failed to open a pseudo-terminal: {e}"))?;

    let mut cmd = CommandBuilder::new(runtime);
    cmd.args(args);
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

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to read the app's output: {e}"))?;
    let console_out = console.clone();
    std::thread::spawn(move || pump(reader, console_out));

    // Wait for the app in the background, holding the master open for its whole lifetime (dropping it
    // early would SIGHUP the app), then note the exit code on the Output tab.
    let console_wait = console.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        drop(pair.master);
        if let Ok(status) = status {
            console_wait.run(&format!("\n[app exited: {}]\n", status.exit_code()));
        }
    });
    Ok(())
}

/// Windows: launch the app with piped stdio.
///
/// Deliberately *not* a pseudo-terminal, unlike Unix. A console program started under a ConPTY
/// blocks before it executes a single instruction: opening one makes conhost ask the terminal where
/// the cursor is (`ESC[6n`) and hold the child until something answers. The Hub only ever reads from
/// the pty — it is not a terminal emulator and has no cursor to report — so nothing ever answers and
/// the app hangs forever, having produced no output and no window. That is precisely the symptom
/// this exists to avoid: the launch line printed and then nothing, with no error to explain it.
///
/// The cost is the app's ANSI colour, which it turns off when it sees a pipe — the same trade the
/// build tools already make on every platform. Running is worth more than colour.
#[cfg(windows)]
fn launch(console: &Console, runtime: &Path, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let mut cmd = Command::new(runtime);
    cmd.args(args)
        // The Hub is a GUI process with no console of its own, so there is no stdin to inherit.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The runtime is a console program: without this it opens a black console window next to
        // the app's own window. Its output is piped here either way, so nothing is lost — the same
        // reasoning as `external_command`.
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch runtime {}: {e}", runtime.display()))?;

    // Two pipes rather than a pty's single merged stream, so each needs its own pump. Both land on
    // the same Output tab, which is how it read before.
    if let Some(out) = child.stdout.take() {
        let console = console.clone();
        std::thread::spawn(move || pump(out, console));
    }
    if let Some(err) = child.stderr.take() {
        let console = console.clone();
        std::thread::spawn(move || pump(err, console));
    }

    // As on Unix, the app outlives the job that launched it: wait in the background, then note the
    // exit code on the project's Output tab.
    let console_wait = console.clone();
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            console_wait.run(&format!("\n[app exited: {}]\n", status.code().unwrap_or(-1)));
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
    let args = vec![lib.to_string_lossy().into_owned()];
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
fn run_step(console: &Console, cmd: &mut Command) -> Result<(), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to launch {:?}: {e}", cmd.get_program()))?;

    if !output.stdout.is_empty() {
        console.build(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        console.build(&String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        return Err(format!("command failed ({})", output.status));
    }
    Ok(())
}

/// Windows: the tools the Hub spawns must be able to find the compiler.
///
/// This is the whole reason `msvc::environment` exists. Ninja will not go looking for MSVC the way
/// the Visual Studio generator does, so if a child of `external_command` cannot resolve `cl.exe`,
/// every build fails with "No CMAKE_CXX_COMPILER could be found" — and it fails inside CMake, far
/// from the cause.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn a_spawned_tool_can_find_the_compiler() {
        if crate::msvc::environment().is_empty() && crate::ide::which("cl").is_none() {
            return; // no C++ toolchain installed here — nothing this test can assert
        }
        let out = external_command("cmd")
            .args(["/c", "where cl"])
            .output()
            .expect("cmd should run");
        let found = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && found.to_lowercase().contains("cl.exe"),
            "a spawned tool should resolve cl.exe, got: {found:?}"
        );
    }
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
