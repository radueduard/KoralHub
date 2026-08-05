//! Build-scaffolding generation.
//!
//! The Hub owns the build files and regenerates them from the portable `koral.json` plus the
//! resolved SDK, on every build and every "Open in IDE". **None of them is committed.**
//! `CMakePresets.json` could not be — it bakes in this machine's absolute SDK paths — and while
//! `CMakeLists.txt` and `vcpkg.json` are portable, a copy of either in git is only ever a stale
//! duplicate of the `koral.json` it was derived from, churning on every change to the module list.
//! `koral.json` is the source of truth; see the `.gitignore` template in `project`.
//!
//! The conventions here (imported target `Koral::Koral`, the SDK's `cmakeDir`) are the
//! contract a Koral SDK release must satisfy.
//!
//! vcpkg is opt-in, driven by the project's own `libraries`. A project that needs no external
//! packages — the common case, since the SDK vendors everything its public headers expose — gets
//! no `vcpkg.json` and no `CMAKE_TOOLCHAIN_FILE`, and does not need vcpkg installed at all. Only
//! a project that names extra ports pulls it in. See [`vcpkg_toolchain`].

use std::path::Path;

use serde_json::{json, Value};

use crate::framework::FrameworkManifest;
use crate::model::ProjectConfig;

/// Per-profile build directory, e.g. `cmake-build-debug`.
pub fn build_dir_name(profile: &str) -> String {
    format!("cmake-build-{}", profile.to_lowercase())
}

/// (Re)generate CMakeLists.txt, CMakePresets.json and — only when the project actually needs
/// vcpkg — vcpkg.json.
pub fn generate(
    project_root: &Path,
    cfg: &ProjectConfig,
    sdk_root: &Path,
    manifest: &FrameworkManifest,
    profile: &str,
) -> Result<(), String> {
    let modules = crate::modules::build_inputs(&cfg.modules, sdk_root);
    write(
        project_root.join("CMakeLists.txt"),
        &cmakelists(&cfg.name, &modules.sdk_targets),
    )?;

    let vcpkg = vcpkg_toolchain(cfg)?;
    let manifest_file = project_root.join("vcpkg.json");
    if vcpkg.is_some() {
        write(manifest_file, &vcpkg_json(cfg, manifest))?;
    } else {
        // No external packages — so no vcpkg manifest. Delete one left behind by a project that
        // used to declare libraries (or by an older Hub that wrote one unconditionally); CMake
        // would otherwise still find it and drag vcpkg back into a build that does not need it.
        let _ = std::fs::remove_file(manifest_file);
    }

    let runtime = sdk_root.join(&manifest.runtime);
    let generator = preferred_generator();
    write(
        project_root.join("CMakePresets.json"),
        &presets_json(
            sdk_root,
            manifest,
            profile,
            vcpkg.as_deref(),
            &runtime,
            &generator,
            &modules.include_dirs,
        ),
    )?;
    clear_foreign_build_dir(project_root, profile, &generator);

    ide_configs(project_root, cfg, profile, &runtime, sdk_root)?;
    Ok(())
}

/// Where a debugger should look for source that is *not* this project's own: the framework, when
/// it is a build from source, and every module project this project loads.
///
/// This is what turns a crash inside the engine from an address into a line of code. Nothing here
/// exists for a downloaded SDK — a release ships no source — so a project on a published framework
/// gets exactly the configuration it had before.
fn debug_source_dirs(cfg: &ProjectConfig) -> Vec<std::path::PathBuf> {
    let mut dirs = crate::framework::debug_source_dirs(&cfg.framework_version);
    dirs.extend(crate::modules::debug_source_dirs(&cfg.modules));
    dirs
}

/// The CMake generator to pin in the preset.
///
/// Pinning one is a correctness requirement, not a preference: CMake's platform default is
/// "Unix Makefiles" (or Visual Studio), while CLion defaults to Ninja. Left unpinned, whichever
/// tool configures the build directory first wins and the other refuses to touch it — "created
/// with incompatible generator". Naming it in the preset makes the Hub, VS Code and CLion agree.
///
/// Ninja when it is installed (what CLion wants, and faster); otherwise leave the field out and
/// let CMake pick its default.
fn preferred_generator() -> Option<String> {
    crate::ide::which("ninja").map(|_| "Ninja".to_string())
}

/// Delete a build directory that was configured with a *different* generator.
///
/// CMake cannot switch a build tree's generator in place — it errors and tells the user to
/// delete the directory by hand. The Hub owns this directory, so it does that itself. Without
/// this, the first build after the generator is pinned fails for every project that already has
/// a Makefiles tree on disk.
fn clear_foreign_build_dir(project_root: &Path, profile: &str, generator: &Option<String>) {
    let Some(want) = generator else {
        return;
    };
    let build_dir = project_root.join(build_dir_name(profile));
    let Ok(text) = std::fs::read_to_string(build_dir.join("CMakeCache.txt")) else {
        return; // never configured — nothing to clash with
    };

    let current = text
        .lines()
        .find_map(|l| l.strip_prefix("CMAKE_GENERATOR:INTERNAL="))
        .map(str::trim);

    if current.is_some_and(|c| c != want) {
        let _ = std::fs::remove_dir_all(&build_dir);
    }
}

/// Write the VS Code and CLion configuration that makes Run/Debug work inside the IDE.
///
/// Committed and portable: `.vscode/tasks.json` (drives the CMake preset) and
/// `.vscode/extensions.json`. Machine-local and git-ignored: `.vscode/launch.json`,
/// `.vscode/c_cpp_properties.json`, `.koral/` and `.idea/` — all of which name absolute paths
/// (the SDK runtime, and the framework/module source trees a debugger steps into).
fn ide_configs(
    project_root: &Path,
    cfg: &ProjectConfig,
    profile: &str,
    runtime: &Path,
    sdk_root: &Path,
) -> Result<(), String> {
    let sources = debug_source_dirs(cfg);
    let gdb_script = write_gdb_script(project_root, &sources)?;

    let vscode = project_root.join(".vscode");
    std::fs::create_dir_all(&vscode).map_err(|e| e.to_string())?;
    write(vscode.join("tasks.json"), &vscode_tasks(profile))?;
    write(vscode.join("settings.json"), &vscode_settings(profile))?;
    write(vscode.join("extensions.json"), VSCODE_EXTENSIONS)?;
    write(
        vscode.join("launch.json"),
        &vscode_launch(cfg, profile, runtime, gdb_script.as_deref()),
    )?;

    // Only when there is external source to browse. Left absent otherwise, so a project on a
    // published SDK keeps taking its IntelliSense configuration purely from `settings.json` and
    // CMake Tools, exactly as before.
    let properties = vscode.join("c_cpp_properties.json");
    if sources.is_empty() {
        let _ = std::fs::remove_file(&properties);
    } else {
        write(properties, &vscode_cpp_properties(profile, &sources, sdk_root))?;
    }

    let clion = project_root.join(".idea").join("runConfigurations");
    std::fs::create_dir_all(&clion).map_err(|e| e.to_string())?;
    write(
        clion.join(format!("Koral_{profile}.xml")),
        &clion_run_config(cfg, profile, runtime),
    )?;
    // The run configuration alone is not enough: it names a CMake profile, and CLion has to be
    // building with that profile rather than one it invented for itself.
    write_clion_profile(project_root, profile)?;

    // `/.koral/` is in the template a fresh project gets, but a project scaffolded before the
    // debug script existed has no rule for it — and would commit a file full of this machine's
    // absolute paths.
    ensure_ignored(
        project_root,
        &[
            "/.idea/",
            "/.vscode/launch.json",
            "/.vscode/c_cpp_properties.json",
            "/.koral/",
            // Generated from koral.json on every build, so a committed copy is only ever a stale
            // duplicate of it. Older projects committed these before that was settled; the rule
            // stops the churn, but untracking an already-committed copy is the user's call —
            // `git rm --cached` is not something the Hub should do to a repository behind them.
            "/CMakeLists.txt",
            "/vcpkg.json",
        ],
    )
}

/// Write the gdb script that puts the framework's and modules' sources on the debugger's search
/// path, and return its absolute path (`None` when there is nothing to search).
///
/// A `directory` entry is what lets gdb open a file whose recorded build path no longer exists —
/// a source tree that has since moved, or an SDK installed from a build directory that was then
/// cleaned. When the tree *is* still where it was built, gdb finds it without help and these
/// entries change nothing, so this is always safe to emit.
///
/// A file rather than inline commands so it can be handed to a debugger that is not being driven
/// from here: VS Code sources it from `launch.json` (below), and a bare `gdb` takes it with `-x`.
///
/// **CLion is not wired up to it**, and cannot be: a startup script can only be attached to its
/// embedded/GDB-Server run configurations, not to the local CMake Application one the Hub writes.
/// It costs CLion nothing today — a source tree still sitting where it was built is found from the
/// absolute paths in the debug info, with no search path involved — but a tree that has moved is
/// resolvable from VS Code and not from CLion.
fn write_gdb_script(project_root: &Path, sources: &[std::path::PathBuf]) -> Result<Option<String>, String> {
    let dir = project_root.join(".koral");
    let file = dir.join("debug.gdb");
    if sources.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Ok(None);
    }

    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut script = String::from(
        "# Generated by Koral Hub — regenerated on every build, so do not edit.\n\
         # Source for the framework and the modules this project loads, so a frame inside either\n\
         # opens the code that failed instead of a bare address.\n",
    );
    for source in sources {
        script.push_str(&format!("directory {}\n", cmake_path(source)));
    }
    write(file.clone(), &script)?;
    Ok(Some(cmake_path(&file)))
}

/// Append ignore rules the project does not have yet, leaving the rest of the file alone.
///
/// Existing projects were scaffolded before these files existed, so their `.gitignore` predates
/// them; rewriting the whole file would clobber anything the user added.
fn ensure_ignored(project_root: &Path, rules: &[&str]) -> Result<(), String> {
    let file = project_root.join(".gitignore");
    let current = std::fs::read_to_string(&file).unwrap_or_default();

    let missing: Vec<&str> = rules
        .iter()
        .filter(|r| !current.lines().any(|l| l.trim() == **r))
        .copied()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut out = current;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# Generated by Koral Hub — regenerated on every build, so never committed\n");
    for rule in missing {
        out.push_str(rule);
        out.push('\n');
    }
    std::fs::write(&file, out).map_err(|e| format!("failed to write {}: {e}", file.display()))
}

/// Absolute path to vcpkg's CMake toolchain file, or `None` when the project does not need
/// vcpkg at all.
///
/// **vcpkg is opt-in, and opting in is the project's call, not the SDK's.** A project that
/// declares no `libraries` needs no package manager: the SDK vendors everything its public
/// headers expose and hands it over through `Koral::Koral`. Then no `vcpkg.json` and no
/// `CMAKE_TOOLCHAIN_FILE` are written, and vcpkg need not be installed at all.
///
/// Only a project naming extra ports pulls vcpkg in — and only then is `VCPKG_ROOT` genuinely
/// required, reported here with the ports that caused it, rather than surfacing later as CMake
/// failing to find `/scripts/buildsystems/vcpkg.cmake` (what an unset `$env{VCPKG_ROOT}` expands
/// to) with no hint as to why vcpkg was involved.
fn vcpkg_toolchain(cfg: &ProjectConfig) -> Result<Option<String>, String> {
    if cfg.libraries.is_empty() {
        return Ok(None);
    }
    let ports: Vec<&str> = cfg.libraries.iter().map(|l| l.vcpkg_port.as_str()).collect();
    let root = std::env::var("VCPKG_ROOT").map_err(|_| {
        format!(
            "this project needs vcpkg for {}, but VCPKG_ROOT is not set — install vcpkg and point \
             VCPKG_ROOT at it, or remove those libraries from koral.json",
            ports.join(", ")
        )
    })?;
    Ok(Some(cmake_path(
        &Path::new(&root).join("scripts/buildsystems/vcpkg.cmake"),
    )))
}

fn write(path: std::path::PathBuf, contents: &str) -> Result<(), String> {
    std::fs::write(&path, contents).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// CMake paths use forward slashes on every platform to dodge JSON/CMake backslash escaping.
fn cmake_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The generated CMakeLists carries no run settings at all — see the `run` target in the template.
///
/// `sdk_module_targets` are the exported targets of the SDK modules this project has checked. They
/// go in *this* file rather than the preset because a target name is portable: it means the same
/// thing on every machine, and a machine whose SDK lacks one should fail loudly rather than build
/// a library that cannot load.
fn cmakelists(name: &str, sdk_module_targets: &[String]) -> String {
    let links = if sdk_module_targets.is_empty() {
        String::new()
    } else {
        format!(
            "\n# Modules this project checked, each its own library with its own headers. Linking is\n\
             # what puts a module's include directory on this build — so a module that is *not*\n\
             # checked contributes nothing, and using its header is a build error rather than a\n\
             # \"module not found\" at launch.\ntarget_link_libraries({{NAME}} PRIVATE\n    {})\n",
            sdk_module_targets.join("\n    ")
        )
    };
    CMAKELISTS_TEMPLATE
        .replace("{MODULE_LINKS}", &links)
        .replace("{NAME}", name)
}

/// VS Code build/run tasks. Portable — they drive the CMake preset and name no absolute path,
/// so this file is safe to commit and works on a teammate's machine.
fn vscode_tasks(profile: &str) -> String {
    let doc = json!({
        "version": "2.0.0",
        "tasks": [
            {
                "label": "Koral: Build",
                "type": "shell",
                "command": "cmake",
                "args": ["--build", "--preset", profile],
                "group": { "kind": "build", "isDefault": true },
                "problemMatcher": ["$gcc"]
            },
            {
                // Builds the scene library, then launches it in the SDK runtime. The `run`
                // target is defined by the generated CMakeLists.
                "label": "Koral: Run",
                "type": "shell",
                "command": "cmake",
                "args": ["--build", "--preset", profile, "--target", "run"],
                "problemMatcher": ["$gcc"]
            }
        ]
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// VS Code editor settings — what makes autocomplete for `kor::` types actually work.
///
/// IntelliSense needs to know the compiler's real include paths, and the only place those exist
/// is the compile database CMake writes into the build directory (it carries `-isystem` flags for
/// the SDK's `include/` and `include/koral-vendor/`). Two independent routes are configured, so
/// completion works whether or not CMake Tools is installed:
///
/// - `configurationProvider` — CMake Tools feeds cpptools the flags directly. Preferred, and the
///   only route that works with the Visual Studio generators, which emit no compile database.
/// - `compileCommands` — cpptools reads `compile_commands.json` itself. The fallback, and what
///   makes a fresh clone work before CMake Tools has configured anything.
///
/// Portable: paths are relative to `${workspaceFolder}`, so this file is committed.
fn vscode_settings(profile: &str) -> String {
    let build_dir = build_dir_name(profile);
    let doc = json!({
        "cmake.useCMakePresets": "always",
        "C_Cpp.default.configurationProvider": "ms-vscode.cmake-tools",
        "C_Cpp.default.compileCommands":
            format!("${{workspaceFolder}}/{build_dir}/compile_commands.json"),
        "C_Cpp.default.cppStandard": "c++23",
        // The build tree is large and machine-local; indexing or searching it is pure noise.
        "files.exclude": { format!("{build_dir}"): true }
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// VS Code debug config: attach a debugger to the SDK runtime, with the scene library as its
/// argument. Names the runtime by absolute path, so this file is machine-local (git-ignored).
///
/// `gdb_script` is the source-search script from [`write_gdb_script`]; sourcing it is what makes a
/// crash inside the framework or a module land on the line that failed rather than on an address.
fn vscode_launch(
    cfg: &ProjectConfig,
    profile: &str,
    runtime: &Path,
    gdb_script: Option<&str>,
) -> String {
    let build_dir = build_dir_name(profile);
    let lib = format!(
        "${{workspaceFolder}}/{build_dir}/{}",
        crate::builder::lib_file_name(&cfg.name)
    );
    // The library is the only argument, and there is no environment to set: the runtime reads the
    // project's koral.json for its API, window and content directories. See `builder::runtime_args`.
    let args = crate::builder::runtime_args(Path::new(&lib));

    // cppvsdbg is MSVC-only and takes no MIMode; cppdbg drives gdb/lldb everywhere else.
    let mut config = serde_json::Map::new();
    config.insert("name".into(), json!(format!("Koral: Debug {}", cfg.name)));
    config.insert("request".into(), json!("launch"));
    config.insert("program".into(), json!(cmake_path(runtime)));
    config.insert("args".into(), json!(args));
    config.insert("cwd".into(), json!("${workspaceFolder}"));
    config.insert("stopAtEntry".into(), json!(false));
    config.insert("preLaunchTask".into(), json!("Koral: Build"));
    if cfg!(windows) {
        config.insert("type".into(), json!("cppvsdbg"));
    } else {
        config.insert("type".into(), json!("cppdbg"));
        let gdb = !cfg!(target_os = "macos");
        config.insert("MIMode".into(), json!(if gdb { "gdb" } else { "lldb" }));

        // The module libraries are loaded at runtime from beside the scene library. Naming their
        // build directories lets the debugger resolve their symbols the moment they are loaded,
        // rather than only if it happens to look in the right place.
        let mut so_paths = vec![format!("${{workspaceFolder}}/{build_dir}")];
        so_paths.extend(
            crate::modules::debug_library_dirs(&cfg.modules, profile)
                .iter()
                .map(|p| cmake_path(p)),
        );
        config.insert("additionalSOLibSearchPath".into(), json!(so_paths.join(":")));

        // lldb does not understand a gdb script, and has no equivalent that works without knowing
        // the original build paths — so this is gdb-only, which is Linux and Windows/MinGW.
        if let (true, Some(script)) = (gdb, gdb_script) {
            config.insert(
                "setupCommands".into(),
                json!([{
                    "description": "Search the framework and module sources Koral Hub knows about",
                    "text": format!("source {script}"),
                    "ignoreFailures": true
                }]),
            );
        }
    }

    let doc = json!({ "version": "0.2.0", "configurations": [config] });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// IntelliSense configuration naming the *external* sources this project debugs into — the
/// framework's own tree when it is a source build, and any module projects it loads.
///
/// Written only when there is such source (see [`ide_configs`]), because it exists for one reason:
/// stepping into `kor::` code should land in an editor that can also navigate it. Machine-local
/// and git-ignored — every path in it is absolute.
///
/// It repeats `configurationProvider` and `compileCommands` from `settings.json` on purpose: when
/// this file is present cpptools takes its configuration from here, so leaving them out would
/// quietly undo the compile-database wiring that makes completion work at all.
fn vscode_cpp_properties(profile: &str, sources: &[std::path::PathBuf], sdk_root: &Path) -> String {
    let build_dir = build_dir_name(profile);
    // The browse path is what "go to definition" walks; the SDK's headers come with the compile
    // database, but its sources (when we have them) do not.
    let mut browse: Vec<String> = vec!["${workspaceFolder}".to_string()];
    browse.extend(sources.iter().map(|p| cmake_path(p)));
    browse.push(cmake_path(&sdk_root.join("include")));

    let name = if cfg!(windows) {
        "Win32"
    } else if cfg!(target_os = "macos") {
        "Mac"
    } else {
        "Linux"
    };

    let doc = json!({
        "version": 4,
        "configurations": [{
            "name": name,
            "configurationProvider": "ms-vscode.cmake-tools",
            "compileCommands":
                format!("${{workspaceFolder}}/{build_dir}/compile_commands.json"),
            "cppStandard": "c++23",
            "browse": { "path": browse, "limitSymbolsToIncludedHeaders": false }
        }]
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// The name CLion gives the CMake profile it derives from our configure preset.
///
/// CLion joins the preset's name and its configuration with " - ". Ours are all named after the
/// profile — the configure preset, the build preset, and the configuration it selects — so this
/// is `"Debug - Debug"` whichever pair CLion is actually joining.
///
/// It is not cosmetic: a run configuration's `CONFIG_NAME` has to name the profile it builds
/// with, and the workspace selects the active profile by this same name.
fn clion_profile_name(profile: &str) -> String {
    format!("{profile} - {profile}")
}

/// A CLion run configuration that builds the scene target and launches the SDK runtime on it.
///
/// `RUN_PATH` is CLion's "Executable: select other…" override — needed because the project's own
/// CMake target is a shared library and cannot be run directly. `TARGET_NAME` is what gets built
/// first, via the BuildBeforeRunTask below. `CONFIG_NAME` is the CMake profile it builds in, which
/// is the preset-derived one [`write_clion_profile`] enables — not the plain `Debug` profile CLion
/// would otherwise make for itself.
fn clion_run_config(cfg: &ProjectConfig, profile: &str, runtime: &Path) -> String {
    let build_dir = build_dir_name(profile);
    let lib = format!(
        "$PROJECT_DIR$/{build_dir}/{}",
        crate::builder::lib_file_name(&cfg.name)
    );
    // The library alone, and no <envs> block: the runtime takes its API, window and content
    // directories from the project's koral.json. See `builder::runtime_args`.
    let params = crate::builder::runtime_args(Path::new(&lib));

    format!(
        r#"<component name="ProjectRunConfigurationManager">
  <configuration default="false" name="Koral: {name} ({profile})" type="CMakeRunConfiguration" factoryName="Application" PROGRAM_PARAMS="{params}" REDIRECT_INPUT="false" ELEVATE="false" USE_EXTERNAL_CONSOLE="false" EMULATE_TERMINAL="false" PASS_PARENT_ENVS_2="true" PROJECT_NAME="{name}" TARGET_NAME="{name}" CONFIG_NAME="{config}" RUN_PATH="{runtime}" WORKING_DIR="$PROJECT_DIR$">
    <method v="2">
      <option name="com.jetbrains.cidr.execution.CidrBuildBeforeRunTaskProvider$BuildBeforeRunTask" enabled="true" />
    </method>
  </configuration>
</component>
"#,
        name = cfg.name,
        profile = profile,
        config = xml_attr(&clion_profile_name(profile)),
        params = xml_attr(&params.join(" ")),
        runtime = xml_attr(&cmake_path(runtime)),
    )
}

/// Make CLion build through the Hub's CMake preset instead of a profile of its own.
///
/// Left alone, CLion creates a plain `Debug` profile the first time it loads the project and
/// generates it into `cmake-build-debug` — the very directory the preset uses. Two owners, one
/// build tree: CLion reconfigures without `CMAKE_PREFIX_PATH` or `KORAL_RUNTIME` and then inherits
/// whatever the Hub last left in the cache, so the IDE and the Hub's ▶ quietly stop building the
/// same thing. The preset profile is right there in the list, just switched off.
///
/// So the Hub enables the preset profile, selects it, and turns CLion's own off.
///
/// These live in `.idea/workspace.xml`, which is **CLion's** file, not ours: this replaces the two
/// components that decide which profile is active and leaves every other byte alone. A file whose
/// shape we don't recognise is left completely untouched rather than rewritten.
///
/// Two consequences of it being CLion's file. It is only written when it is actually wrong, so a
/// rebuild is not a fight over it. And CLion wins whenever it has the project open — it holds this
/// state in memory and writes it back on save — which is why this runs before the IDE is launched
/// (see `ide::open`), when it is still read on load.
fn write_clion_profile(project_root: &Path, profile: &str) -> Result<(), String> {
    let idea = project_root.join(".idea");
    let file = idea.join("workspace.xml");
    let current = std::fs::read_to_string(&file).unwrap_or_default();

    let base = if current.trim().is_empty() {
        EMPTY_WORKSPACE.to_string()
    } else {
        current.clone()
    };

    let patched = set_component(&base, "CMakeSettings", &clion_cmake_settings(profile))
        .and_then(|xml| {
            set_component(
                &xml,
                "ExecutionTargetManager",
                &clion_selected_profile(profile),
            )
        });
    // Not a shape we understand — a file with no `</project>`, or an unclosed component. Better a
    // CLion that picks its own profile than a CLion whose state file we corrupted.
    let Some(patched) = patched else {
        return Ok(());
    };

    if patched == current {
        return Ok(());
    }
    std::fs::create_dir_all(&idea).map_err(|e| e.to_string())?;
    write(file, &patched)
}

/// What CLion writes for a project it has never opened.
const EMPTY_WORKSPACE: &str =
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n</project>";

/// The CMake profile list: ours enabled, CLion's own default switched off.
///
/// The plain profile is named rather than dropped on purpose — leaving it out is not how CLion
/// records "off", and it would simply be recreated, enabled, on the next reload.
fn clion_cmake_settings(profile: &str) -> String {
    format!(
        r#"  <component name="CMakeSettings">
    <configurations>
      <configuration PROFILE_NAME="{profile}" ENABLED="false" CONFIG_NAME="{profile}" />
      <configuration PROFILE_NAME="{preset}" ENABLED="true" FROM_PRESET="true" GENERATION_DIR="$PROJECT_DIR$/{build_dir}" />
    </configurations>
  </component>"#,
        profile = xml_attr(profile),
        preset = xml_attr(&clion_profile_name(profile)),
        build_dir = build_dir_name(profile),
    )
}

/// The profile the toolbar builds and runs with.
fn clion_selected_profile(profile: &str) -> String {
    format!(
        r#"  <component name="ExecutionTargetManager" SELECTED_TARGET="CMakeBuildProfile:{preset}" />"#,
        preset = xml_attr(&clion_profile_name(profile)),
    )
}

/// Replace one `<component name="…">` element in a JetBrains state file, or add it before
/// `</project>` when it is not there yet. `None` when the file is not the shape this expects,
/// which is the signal to leave it alone.
///
/// A splice rather than an XML round trip: everything else in the file belongs to CLion, and
/// rewriting it through a serializer would reformat state we have no business touching.
/// Components in these files never nest, so the first `</component>` after the opening tag closes
/// it. (An attribute value containing `>` would fool the self-closing check — JetBrains writes
/// none, and the worst case is that we decline to touch the file.)
fn set_component(xml: &str, name: &str, replacement: &str) -> Option<String> {
    // Includes the closing quote, so "CMakeSettings" cannot match "CMakeSettingsSomethingElse".
    let opening = format!("<component name=\"{name}\"");

    let (from, to) = match xml.find(&opening) {
        Some(start) => {
            let rest = &xml[start..];
            let tag_end = rest.find('>')?;
            let end = if rest[..tag_end].ends_with('/') {
                start + tag_end + 1
            } else {
                start + rest.find("</component>")? + "</component>".len()
            };
            // Back up over the element's own indentation: the replacement carries its own.
            (line_start(xml, start), end)
        }
        // Insert on the line the closing tag is on, pushing it down.
        None => {
            let close = line_start(xml, xml.rfind("</project>")?);
            (close, close)
        }
    };

    let mut out = String::with_capacity(xml.len() + replacement.len());
    out.push_str(&xml[..from]);
    out.push_str(replacement);
    if from == to {
        out.push('\n');
    }
    out.push_str(&xml[to..]);
    Some(out)
}

/// The offset of the start of the line `at` falls on.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Escape a string for use inside a double-quoted XML attribute.
fn xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Extensions VS Code needs for the generated tasks and debug config to work. Recommended
/// rather than required — VS Code prompts once, and nothing else has to be installed by hand.
const VSCODE_EXTENSIONS: &str = r#"{
  "recommendations": [
    "ms-vscode.cpptools",
    "ms-vscode.cmake-tools"
  ]
}
"#;

/// Only written for a project that actually declares libraries — see [`vcpkg_toolchain`].
fn vcpkg_json(cfg: &ProjectConfig, manifest: &FrameworkManifest) -> String {
    // A version constraint without a baseline is a hard vcpkg error ("no baseline for versioned
    // dependency"), and current SDKs publish no baseline. So the two travel together: pin
    // versions only when the SDK told us which universe of versions to pin against.
    let baseline = &manifest.vcpkg_baseline;

    let deps: Vec<Value> = cfg
        .libraries
        .iter()
        .map(|l| {
            let mut e = serde_json::Map::new();
            e.insert("name".into(), json!(l.vcpkg_port));
            if !l.min_version.is_empty() && !baseline.is_empty() {
                e.insert("version>=".into(), json!(l.min_version));
            }
            if !l.features.is_empty() {
                e.insert("features".into(), json!(l.features));
            }
            Value::Object(e)
        })
        .collect();

    let mut doc = serde_json::Map::new();
    doc.insert("name".into(), json!(cfg.name.to_lowercase()));
    doc.insert("version-string".into(), json!("1.0.0"));
    if !baseline.is_empty() {
        // Inherited from the SDK so the project resolves ports at the exact ABI the framework's
        // public headers were built against.
        doc.insert("builtin-baseline".into(), json!(baseline));
    }
    doc.insert("dependencies".into(), json!(deps));

    serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default()
}

fn presets_json(
    sdk_root: &Path,
    manifest: &FrameworkManifest,
    profile: &str,
    vcpkg_toolchain: Option<&str>,
    runtime: &Path,
    generator: &Option<String>,
    module_includes: &[std::path::PathBuf],
) -> String {
    let sdk_cmake = cmake_path(&sdk_root.join(&manifest.cmake_dir));
    let build_dir = build_dir_name(profile);

    let mut cache = serde_json::Map::new();
    cache.insert("CMAKE_BUILD_TYPE".into(), json!(profile));
    // The SDK's package-config dir, so find_package(Koral) resolves it.
    cache.insert("CMAKE_PREFIX_PATH".into(), json!(sdk_cmake));
    // Where the scene runtime lives, so CMakeLists can define a `run` target without naming an
    // absolute path itself. This preset is machine-local and git-ignored; CMakeLists is not.
    cache.insert("KORAL_RUNTIME".into(), json!(cmake_path(runtime)));
    // Header directories of the module *projects* this project checked. Absolute, and so kept
    // here rather than in the committed CMakeLists — the same split that keeps the SDK paths out
    // of it. A project that checked no module projects sets nothing, and the template's guard
    // leaves the include path alone.
    if !module_includes.is_empty() {
        let dirs: Vec<String> = module_includes.iter().map(|p| cmake_path(p)).collect();
        cache.insert("KORAL_MODULE_INCLUDES".into(), json!(dirs.join(";")));
    }
    // The compile database is what gives the editor Koral's headers. Without it, IntelliSense
    // falls back to guessing include paths and autocomplete for `kor::` types silently does not
    // work. Set explicitly rather than relying on a generator's default. (The Visual Studio
    // generators cannot emit one and ignore this; there, cpptools uses the CMake Tools provider.)
    cache.insert("CMAKE_EXPORT_COMPILE_COMMANDS".into(), json!("ON"));
    // Resolved to a real path by `vcpkg_toolchain`, and omitted entirely when the SDK vendors
    // its own dependencies — an unset $env{VCPKG_ROOT} would otherwise expand to garbage.
    if let Some(toolchain) = vcpkg_toolchain {
        cache.insert("CMAKE_TOOLCHAIN_FILE".into(), json!(toolchain));
    }
    // Link the *release* C runtime even in Debug, on Windows only.
    //
    // MSVC cannot mix CRTs in one image, and the SDK publishes a single build of its vendored
    // static libraries (`fmt.lib`, …) compiled against the release CRT. A Debug project would
    // otherwise default to `/MDd` and fail to link with LNK2038 on both `RuntimeLibrary` and
    // `_ITERATOR_DEBUG_LEVEL` — two faces of the same mismatch, since `/MD` leaves `_DEBUG`
    // undefined and the iterator level follows it. Choosing `/MD` costs the debug heap and
    // iterator debugging; optimisation and debug info are set by the build type and unaffected,
    // so debugging still works. The real fix is for the SDK to ship a debug set of vendored
    // libraries, at which point this goes away.
    if cfg!(windows) {
        cache.insert("CMAKE_MSVC_RUNTIME_LIBRARY".into(), json!("MultiThreadedDLL"));
    }

    let mut configure = serde_json::Map::new();
    configure.insert("name".into(), json!(profile));
    configure.insert(
        "binaryDir".into(),
        json!(format!("${{sourceDir}}/{build_dir}")),
    );
    // Pinned so the Hub, VS Code and CLion do not each pick a different one and then refuse to
    // share the build directory. Omitted only when the preferred generator is unavailable.
    if let Some(generator) = generator {
        configure.insert("generator".into(), json!(generator));
    }
    configure.insert("cacheVariables".into(), Value::Object(cache));

    let doc = json!({
        "version": 4,
        "configurePresets": [configure],
        "buildPresets": [{
            "name": profile,
            "configurePreset": profile,
            // Multi-config generators ignore CMAKE_BUILD_TYPE and pick their own default
            // (Debug), so without this a Release build silently produces Debug binaries.
            // Single-config generators ignore it in turn, having already baked the type in.
            "configuration": profile
        }]
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> FrameworkManifest {
        FrameworkManifest {
            name: "koral".into(),
            version: "0.0.5".into(),
            platform: "windows-x64".into(),
            runtime: "bin/Koral_Runtime.exe".into(),
            cmake_dir: "lib/cmake/Koral".into(),
            vcpkg_baseline: String::new(),
        }
    }

    fn presets(profile: &str) -> Value {
        let sdk = Path::new("/sdk");
        let text = presets_json(
            sdk,
            &manifest(),
            profile,
            None,
            &sdk.join("bin/Koral_Runtime.exe"),
            &None,
            &[],
        );
        serde_json::from_str(&text).expect("presets must be valid JSON")
    }

    /// Multi-config generators (Visual Studio, and what Windows falls back to without Ninja)
    /// ignore CMAKE_BUILD_TYPE, so the build preset has to name the configuration itself or a
    /// Release build quietly produces Debug binaries.
    #[test]
    fn the_build_preset_names_its_configuration() {
        for profile in ["Debug", "Release"] {
            assert_eq!(
                presets(profile)["buildPresets"][0]["configuration"], profile,
                "{profile}"
            );
        }
    }

    /// The SDK ships its vendored static libraries built against the release CRT only, and MSVC
    /// refuses to mix CRTs — a Debug build defaulting to /MDd fails with LNK2038.
    #[test]
    fn windows_pins_the_release_msvc_runtime() {
        let cache = presets("Debug")["configurePresets"][0]["cacheVariables"].clone();
        if cfg!(windows) {
            assert_eq!(cache["CMAKE_MSVC_RUNTIME_LIBRARY"], "MultiThreadedDLL");
        } else {
            assert!(cache.get("CMAKE_MSVC_RUNTIME_LIBRARY").is_none());
        }
    }

    /// The point of the whole source-path arrangement: with sources to search, the debug config
    /// sources a gdb script that names them, so a frame inside the framework opens its code. With
    /// none — a project on a downloaded SDK, which ships no source — nothing is written and the
    /// config is exactly what it was before.
    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn the_debug_config_searches_the_sources_it_was_given() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("koral-debugcfg-{n}"));
        let engine = root.join("engine-src");
        std::fs::create_dir_all(&engine).unwrap();

        let cfg = ProjectConfig::new("Game", "source", [0.5, 0.5, 0.5], crate::model::Kind::Scene);
        let script = write_gdb_script(&root, &[engine.clone()]).unwrap();
        let script = script.expect("sources were given, so a script must be written");

        let written = std::fs::read_to_string(root.join(".koral/debug.gdb")).unwrap();
        assert!(
            written.contains(&format!("directory {}", engine.display())),
            "the script must put the source tree on gdb's search path: {written}"
        );

        let launch: Value =
            serde_json::from_str(&vscode_launch(&cfg, "Debug", Path::new("/sdk/bin/Koral_Runtime"), Some(&script)))
                .unwrap();
        let setup = &launch["configurations"][0]["setupCommands"][0]["text"];
        assert_eq!(setup, &json!(format!("source {script}")));
        // The scene library's own directory is always searched, so module symbols resolve too.
        assert!(launch["configurations"][0]["additionalSOLibSearchPath"]
            .as_str()
            .unwrap()
            .contains("cmake-build-debug"));

        // Nothing to search: the script is removed, and the config gains no setup commands.
        assert_eq!(write_gdb_script(&root, &[]).unwrap(), None);
        assert!(!root.join(".koral/debug.gdb").exists());
        let plain: Value =
            serde_json::from_str(&vscode_launch(&cfg, "Debug", Path::new("/sdk/bin/Koral_Runtime"), None))
                .unwrap();
        assert!(plain["configurations"][0].get("setupCommands").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A temporary project root, removed by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("koral-{tag}-{n}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The run configuration names the CMake profile it builds with, and that has to be the
    /// preset-derived profile the Hub enables — not the plain one CLion makes for itself, which
    /// configures the same build directory without the SDK paths.
    #[test]
    fn the_run_config_builds_with_the_profile_the_hub_enables() {
        let cfg = ProjectConfig::new("Game", "source", [0.5, 0.5, 0.5], crate::model::Kind::Scene);
        let run = clion_run_config(&cfg, "Debug", Path::new("/sdk/bin/Koral_Runtime"));
        let enabled = clion_cmake_settings("Debug");

        let profile = clion_profile_name("Debug");
        assert_eq!(profile, "Debug - Debug");
        assert!(run.contains(&format!(r#"CONFIG_NAME="{profile}""#)), "{run}");
        assert!(
            enabled.contains(&format!(r#"PROFILE_NAME="{profile}" ENABLED="true""#)),
            "{enabled}"
        );
        assert!(
            clion_selected_profile("Debug")
                .contains(&format!(r#"SELECTED_TARGET="CMakeBuildProfile:{profile}""#))
        );
        // The run configuration is still named for the human, not for CLion's profile.
        assert!(run.contains(r#"name="Koral: Game (Debug)""#), "{run}");
    }

    /// The state CLion lands in by itself, taken verbatim from a project that hit this: its own
    /// `Debug` profile enabled and selected, the preset profile switched off, both generating into
    /// `cmake-build-debug`. The Hub has to turn that around — and leave the rest of CLion's file
    /// exactly as it found it.
    #[test]
    fn clion_is_switched_onto_the_preset_profile() {
        let root = scratch("clion-profile");
        let idea = root.join(".idea");
        std::fs::create_dir_all(&idea).unwrap();
        std::fs::write(
            idea.join("workspace.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="AutoImportSettings">
    <option name="autoReloadType" value="SELECTIVE" />
  </component>
  <component name="CMakeSettings">
    <configurations>
      <configuration PROFILE_NAME="Debug" ENABLED="true" CONFIG_NAME="Debug" />
      <configuration PROFILE_NAME="Debug - Debug" ENABLED="false" FROM_PRESET="true" GENERATION_DIR="$PROJECT_DIR$/cmake-build-debug" />
    </configurations>
  </component>
  <component name="ExecutionTargetManager" SELECTED_TARGET="CMakeBuildProfile:Debug" />
  <component name="VcsManagerConfiguration">
    <option name="ADD_EXTERNAL_FILES_SILENTLY" value="true" />
  </component>
</project>"#,
        )
        .unwrap();

        write_clion_profile(&root, "Debug").unwrap();
        let out = std::fs::read_to_string(idea.join("workspace.xml")).unwrap();

        assert!(
            out.contains(r#"<configuration PROFILE_NAME="Debug - Debug" ENABLED="true" FROM_PRESET="true""#),
            "the preset profile must be enabled: {out}"
        );
        assert!(
            out.contains(r#"<configuration PROFILE_NAME="Debug" ENABLED="false" CONFIG_NAME="Debug" />"#),
            "CLion's own profile must be switched off rather than dropped: {out}"
        );
        assert!(
            out.contains(r#"SELECTED_TARGET="CMakeBuildProfile:Debug - Debug""#),
            "the preset profile must be the selected one: {out}"
        );
        // Everything that is CLion's own state survives untouched.
        assert!(out.contains(r#"<option name="autoReloadType" value="SELECTIVE" />"#), "{out}");
        assert!(out.contains(r#"<option name="ADD_EXTERNAL_FILES_SILENTLY" value="true" />"#), "{out}");
        assert!(out.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#), "{out}");
        assert!(out.trim_end().ends_with("</project>"), "{out}");

        // Already right: the file is left completely alone, so a rebuild with CLion open is not a
        // fight over whose copy of workspace.xml wins.
        let before = std::fs::metadata(idea.join("workspace.xml")).unwrap().modified().unwrap();
        write_clion_profile(&root, "Debug").unwrap();
        assert_eq!(std::fs::read_to_string(idea.join("workspace.xml")).unwrap(), out);
        assert_eq!(
            std::fs::metadata(idea.join("workspace.xml")).unwrap().modified().unwrap(),
            before
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A project CLion has never opened has no workspace.xml at all — the profile still has to be
    /// there before the IDE first loads, or CLion makes its own and we are back to two owners.
    #[test]
    fn a_project_clion_has_never_opened_gets_the_profile_anyway() {
        let root = scratch("clion-fresh");
        write_clion_profile(&root, "Debug").unwrap();

        let out = std::fs::read_to_string(root.join(".idea/workspace.xml")).unwrap();
        assert!(out.contains(r#"PROFILE_NAME="Debug - Debug" ENABLED="true""#), "{out}");
        assert!(out.contains(r#"SELECTED_TARGET="CMakeBuildProfile:Debug - Debug""#), "{out}");
        assert!(out.contains("</project>"), "{out}");

        std::fs::remove_dir_all(&root).ok();
    }

    /// workspace.xml belongs to CLion. A file we cannot parse confidently is left exactly as it
    /// is: a CLion that picks its own profile is a far smaller problem than a corrupted one.
    #[test]
    fn an_unrecognised_workspace_file_is_left_alone() {
        let root = scratch("clion-foreign");
        let idea = root.join(".idea");
        std::fs::create_dir_all(&idea).unwrap();
        let junk = "<project version=\"4\">\n  <component name=\"Half\">\n";
        std::fs::write(idea.join("workspace.xml"), junk).unwrap();

        write_clion_profile(&root, "Debug").unwrap();
        assert_eq!(std::fs::read_to_string(idea.join("workspace.xml")).unwrap(), junk);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A module is a separate project, so only the ones a project *checked* may reach its build.
    ///
    /// Linking the checked module's target is what carries its headers in (CMake propagates the
    /// include directory as a usage requirement), so an unchecked module contributes no target,
    /// no include path, and no symbols — and a source file that includes its header fails to
    /// build rather than loading and dying on an undefined symbol.
    #[test]
    fn only_checked_modules_reach_the_build() {
        let none = cmakelists("Game", &[]);
        assert!(
            !none.contains("Koral::koral-camera"),
            "a project that checked no modules must link none: {none}"
        );
        // The include hook is always present but guarded, so a project with no module projects
        // sets nothing and the guard leaves its include path alone.
        assert!(none.contains("if(KORAL_MODULE_INCLUDES)"));

        let checked = cmakelists("Game", &["Koral::koral-camera".into(), "Koral::koral-mesh".into()]);
        assert!(checked.contains("Koral::koral-camera"), "{checked}");
        assert!(checked.contains("Koral::koral-mesh"), "{checked}");
        // The project's own library must still be the thing being linked into.
        assert!(checked.contains("target_link_libraries(Game PRIVATE\n    Koral::koral-camera"));

        // A module *project*'s headers are absolute paths, so they belong in the machine-local
        // preset — never in the committed CMakeLists.
        let sdk = Path::new("/sdk");
        let text = presets_json(
            sdk,
            &manifest(),
            "Debug",
            None,
            &sdk.join("bin/Koral_Runtime.exe"),
            &None,
            &[std::path::PathBuf::from("/home/u/Koral/MyCameras/src")],
        );
        let cache: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            cache["configurePresets"][0]["cacheVariables"]["KORAL_MODULE_INCLUDES"],
            json!("/home/u/Koral/MyCameras/src")
        );
        assert!(
            !checked.contains("/home/u/Koral"),
            "an absolute path must not reach the committed CMakeLists"
        );

        // Nothing checked, nothing set — not an empty variable that would defeat the guard.
        let bare = presets("Debug");
        assert!(bare["configurePresets"][0]["cacheVariables"]
            .get("KORAL_MODULE_INCLUDES")
            .is_none());
    }

    /// A multi-config generator appends its configuration to the plain output-directory
    /// variables, which is what put the library in `cmake-build-debug/Debug/` and left the Hub
    /// and both IDE configs looking for something that wasn't there. Only the per-config
    /// variables are honoured verbatim, so every configuration must set them.
    #[test]
    fn every_configuration_pins_a_flat_output_directory() {
        let cfg = ProjectConfig::new("KoralProject", "0.0.5", [0.5, 0.5, 0.5], crate::model::Kind::Scene);
        let text = cmakelists(&cfg.name, &[]);

        // The loop the per-config variables are set from must cover every configuration a
        // preset can ask for — a missing one silently reverts to the nested layout.
        let configs = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("foreach(KORAL_CFG "))
            .expect("the template should set output directories per configuration");
        for want in ["DEBUG", "RELEASE", "RELWITHDEBINFO", "MINSIZEREL"] {
            assert!(configs.contains(want), "{want} missing from `{configs}`");
        }

        for kind in ["RUNTIME", "LIBRARY", "ARCHIVE"] {
            assert!(
                text.contains(&format!("CMAKE_{kind}_OUTPUT_DIRECTORY_${{KORAL_CFG}}")),
                "{kind} output directory is not pinned per configuration"
            );
        }
    }
}

const CMAKELISTS_TEMPLATE: &str = r#"cmake_minimum_required(VERSION 3.28)
project({NAME} VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Put the scene library at the top of the build directory on every generator.
#
# Multi-config generators (Visual Studio, Xcode) otherwise append the configuration name, so the
# library lands in `cmake-build-debug/Debug/` instead of `cmake-build-debug/`. Koral Hub, the
# VS Code launch config and the CLion run config all name `<build dir>/<library>` directly, so
# without this the build succeeds and then nothing can find what it produced. The per-config
# variables are the ones that matter — a multi-config generator appends its subdirectory to the
# plain `CMAKE_*_OUTPUT_DIRECTORY` but honours these verbatim.
foreach(KORAL_CFG DEBUG RELEASE RELWITHDEBINFO MINSIZEREL)
    set(CMAKE_RUNTIME_OUTPUT_DIRECTORY_${KORAL_CFG} "${CMAKE_BINARY_DIR}")
    set(CMAKE_LIBRARY_OUTPUT_DIRECTORY_${KORAL_CFG} "${CMAKE_BINARY_DIR}")
    set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY_${KORAL_CFG} "${CMAKE_BINARY_DIR}")
endforeach()
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")
set(CMAKE_LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")

# The Koral SDK is located via CMAKE_PREFIX_PATH, which Koral Hub sets in the generated
# preset. The imported target is expected to propagate the public glm/imgui/spdlog usage
# requirements, so consumers don't find or link them explicitly.
find_package(Koral CONFIG REQUIRED)

file(GLOB_RECURSE SOURCE_FILES CONFIGURE_DEPENDS "src/*.cpp" "src/*.c")
file(GLOB_RECURSE HEADER_FILES CONFIGURE_DEPENDS "src/*.h" "src/*.hpp")

add_library(${PROJECT_NAME} SHARED ${SOURCE_FILES} ${HEADER_FILES})

target_link_libraries(${PROJECT_NAME} PRIVATE Koral::Koral)
{MODULE_LINKS}
# Headers of the module projects this project checked, set by the generated (machine-local)
# CMakePresets.json. Unset when none are checked, which is why this is guarded.
if(KORAL_MODULE_INCLUDES)
    target_include_directories(${PROJECT_NAME} PRIVATE ${KORAL_MODULE_INCLUDES})
endif()

target_compile_definitions(${PROJECT_NAME} PRIVATE
    ASSETS_PATH="${CMAKE_CURRENT_SOURCE_DIR}/assets/"
    SHADERS_PATH="${CMAKE_CURRENT_SOURCE_DIR}/shaders/")

# `cmake --build --preset <profile> --target run` builds the scene and launches it in the SDK
# runtime — the same thing Koral Hub's ▶ does, and what the IDE Run configurations invoke.
#
# KORAL_RUNTIME is set by the generated CMakePresets.json, which is machine-local. Keeping the
# path out of this file is what lets CMakeLists.txt stay committable and portable.
#
# The scene library is the only argument. Every run setting — API, window, and the directories the
# project keeps its assets and shaders in — lives in koral.json, which the runtime finds by walking
# up from the library it is handed. That is why this line does not have to know any of them, and
# why it cannot fall out of step with the Hub.
if(KORAL_RUNTIME)
    add_custom_target(run
        COMMAND "${KORAL_RUNTIME}" "$<TARGET_FILE:${PROJECT_NAME}>"
        DEPENDS ${PROJECT_NAME}
        USES_TERMINAL
        COMMENT "Running ${PROJECT_NAME} in the Koral runtime")
endif()
"#;






