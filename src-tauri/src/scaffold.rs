//! Build-scaffolding generation.
//!
//! The Hub owns the build files and regenerates them from the portable `koral.json` plus
//! the resolved SDK. `CMakeLists.txt` and `vcpkg.json` are portable (committable);
//! `CMakePresets.json` bakes in this machine's absolute SDK paths, so it's regenerated
//! every build and git-ignored.
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
    write(project_root.join("CMakeLists.txt"), &cmakelists(&cfg.name, cfg))?;

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
        &presets_json(sdk_root, manifest, profile, vcpkg.as_deref(), &runtime, &generator),
    )?;
    clear_foreign_build_dir(project_root, profile, &generator);

    ide_configs(project_root, cfg, profile, &runtime)?;
    Ok(())
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
/// `.vscode/extensions.json`. Machine-local and git-ignored: `.vscode/launch.json` and
/// `.idea/`, both of which must name the SDK runtime by absolute path.
fn ide_configs(
    project_root: &Path,
    cfg: &ProjectConfig,
    profile: &str,
    runtime: &Path,
) -> Result<(), String> {
    let vscode = project_root.join(".vscode");
    std::fs::create_dir_all(&vscode).map_err(|e| e.to_string())?;
    write(vscode.join("tasks.json"), &vscode_tasks(profile))?;
    write(vscode.join("settings.json"), &vscode_settings(profile))?;
    write(vscode.join("extensions.json"), VSCODE_EXTENSIONS)?;
    write(
        vscode.join("launch.json"),
        &vscode_launch(cfg, profile, runtime),
    )?;

    let clion = project_root.join(".idea").join("runConfigurations");
    std::fs::create_dir_all(&clion).map_err(|e| e.to_string())?;
    write(
        clion.join(format!("Koral_{profile}.xml")),
        &clion_run_config(cfg, profile, runtime),
    )?;

    ensure_ignored(project_root, &["/.idea/", "/.vscode/launch.json"])
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
    out.push_str("\n# Hub-generated IDE state (absolute paths — regenerated on each build)\n");
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
fn cmakelists(name: &str, _cfg: &ProjectConfig) -> String {
    CMAKELISTS_TEMPLATE.replace("{NAME}", name)
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
fn vscode_launch(cfg: &ProjectConfig, profile: &str, runtime: &Path) -> String {
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
        config.insert(
            "MIMode".into(),
            json!(if cfg!(target_os = "macos") { "lldb" } else { "gdb" }),
        );
    }

    let doc = json!({ "version": "0.2.0", "configurations": [config] });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// A CLion run configuration that builds the scene target and launches the SDK runtime on it.
///
/// `RUN_PATH` is CLion's "Executable: select other…" override — needed because the project's own
/// CMake target is a shared library and cannot be run directly. `TARGET_NAME` is what gets built
/// first, via the BuildBeforeRunTask below.
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
  <configuration default="false" name="Koral: {name} ({profile})" type="CMakeRunConfiguration" factoryName="Application" PROGRAM_PARAMS="{params}" REDIRECT_INPUT="false" ELEVATE="false" USE_EXTERNAL_CONSOLE="false" EMULATE_TERMINAL="false" PASS_PARENT_ENVS_2="true" PROJECT_NAME="{name}" TARGET_NAME="{name}" CONFIG_NAME="{profile}" RUN_PATH="{runtime}" WORKING_DIR="$PROJECT_DIR$">
    <method v="2">
      <option name="com.jetbrains.cidr.execution.CidrBuildBeforeRunTaskProvider$BuildBeforeRunTask" enabled="true" />
    </method>
  </configuration>
</component>
"#,
        name = cfg.name,
        profile = profile,
        params = xml_attr(&params.join(" ")),
        runtime = xml_attr(&cmake_path(runtime)),
    )
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

    /// A multi-config generator appends its configuration to the plain output-directory
    /// variables, which is what put the library in `cmake-build-debug/Debug/` and left the Hub
    /// and both IDE configs looking for something that wasn't there. Only the per-config
    /// variables are honoured verbatim, so every configuration must set them.
    #[test]
    fn every_configuration_pins_a_flat_output_directory() {
        let cfg = ProjectConfig::new("KoralProject", "0.0.5", [0.5, 0.5, 0.5], crate::model::Kind::Scene);
        let text = cmakelists(&cfg.name, &cfg);

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






