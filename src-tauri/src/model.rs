//! Portable project schema — the Koral equivalent of the old `config.gfxproj`.
//!
//! Everything in [`ProjectConfig`] is platform- and machine-independent, so a project
//! cloned from a git link resolves identically on Windows, macOS and Linux. Per-user and
//! per-machine state — the selected build profile, absolute build directories, the chosen
//! IDE — is deliberately kept OUT of this struct and lives in the Hub's local cache instead.
//! That split is what lets "clone the link and run" work across machines.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// A project's committed metadata (`koral.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    /// Schema version, so the Hub can migrate older project files forward.
    pub schema_version: u32,
    pub name: String,
    /// Accent color, linear RGB in [0, 1].
    pub color: [f32; 3],
    /// Koral framework release this project builds against. The Hub resolves this to a
    /// prebuilt, per-platform SDK, downloading it if the machine doesn't have it yet.
    pub framework_version: String,
    /// Which kind of app this is. Decides the scaffolded sources, the entry point the library
    /// exports, and — because a Job has no window — which run settings even apply.
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub rendering: Rendering,
    /// Where the scene's own assets and shaders live, relative to the project root. The runtime
    /// reads this file itself and resolves relative texture/model/shader paths against these
    /// directories, so nothing here has to be passed on a command line.
    #[serde(default)]
    pub paths: Paths,
    /// Extra vcpkg ports the project's own source needs, *beyond* what the SDK already provides.
    ///
    /// Empty for a fresh project, and empty is the common case: the SDK vendors everything its
    /// public headers expose (glm, imgui, spdlog, fmt), so a scene that only uses Koral needs no
    /// package manager at all. This list is what decides whether vcpkg is set up — see
    /// `scaffold::vcpkg_toolchain`. The ABI baseline is NOT stored here; it is inherited from the
    /// resolved SDK's manifest so Hub and framework releases can version independently.
    #[serde(default)]
    pub libraries: Vec<Library>,
}

impl ProjectConfig {
    pub const SCHEMA_VERSION: u32 = 1;

    /// A fresh project pins ImGui's layout file to the project root, matching the runtime's own
    /// default location. Written explicitly so it is visible in `koral.json` — the one place a user
    /// changes it, since the Hub does not expose it in the UI.
    pub const DEFAULT_IMGUI_INI: &str = "imgui.ini";

    /// A fresh project: default rendering settings, and **no** external packages.
    pub fn new(
        name: impl Into<String>,
        framework_version: impl Into<String>,
        color: [f32; 3],
        kind: Kind,
    ) -> Self {
        let mut rendering = Rendering::default();
        rendering.window.imgui_ini = Self::DEFAULT_IMGUI_INI.to_string();
        Self {
            schema_version: Self::SCHEMA_VERSION,
            name: name.into(),
            color,
            framework_version: framework_version.into(),
            kind,
            rendering,
            paths: Paths::default(),
            libraries: Vec::new(),
        }
    }
}

/// The two shapes a Koral app can take.
///
/// The engine picks between them by which symbol it finds exported from the scene library —
/// `CreateScene` or `CreateJob` — so a project is one or the other, never both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Kind {
    /// A realtime app: owns a window, runs an Update/Render loop until closed.
    #[default]
    Scene,
    /// A windowless app: `Run()` once to completion on a headless device context, then exit.
    /// Offscreen rendering, compute, asset processing.
    Job,
}

impl Kind {
    /// Does this kind open a window? Job runs on `Context::InitHeadless`, so the window settings
    /// are meaningless for it — the Hub neither passes nor offers them.
    pub fn has_window(self) -> bool {
        matches!(self, Kind::Scene)
    }
}

/// Where the project's content lives, as a search list rather than a single folder: a project can
/// keep its own `assets/` while also pulling from a shared library of models next door.
///
/// Entries are project-relative (not absolute) so the project stays portable across machines — the
/// runtime resolves them against this file's own directory. They are searched in order, and the
/// engine's built-in content is searched after all of them, so a project can shadow a stock asset
/// by name without losing access to the rest.
///
/// An absolute entry is allowed and used as written; it just makes the project non-portable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paths {
    /// Searched for relative texture and model paths.
    #[serde(default = "default_asset_dirs", alias = "assetsDir",
            deserialize_with = "one_or_many")]
    pub asset_directories: Vec<String>,
    /// Searched for relative shader paths, `#include`s and Slang module imports.
    #[serde(default = "default_shader_dirs", alias = "shadersDir",
            deserialize_with = "one_or_many")]
    pub shader_directories: Vec<String>,
}

fn default_asset_dirs() -> Vec<String> {
    vec!["assets".into()]
}

fn default_shader_dirs() -> Vec<String> {
    vec!["shaders".into()]
}

/// Accept either a bare string or a list of them.
///
/// These were single strings (`"assetsDir": "assets"`) before they became search lists. Projects
/// scaffolded back then still have that shape on disk, and they must keep loading — a project that
/// silently stopped finding its own textures would be a miserable thing to debug. The `alias` above
/// accepts the old key; this accepts the old value.
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(dir) => vec![dir],
        OneOrMany::Many(dirs) => dirs,
    })
}

impl Default for Paths {
    /// Matches the folders `project::create` scaffolds.
    fn default() -> Self {
        Self {
            asset_directories: default_asset_dirs(),
            shader_directories: default_shader_dirs(),
        }
    }
}

/// Ports the SDK already vendors and hands to consumers through `Koral::Koral`.
///
/// A project must not declare these: they arrive with the framework (see `Koral::vendored` in the
/// SDK's `KoralConfig.cmake`), and listing them would drag in vcpkg to resolve packages the build
/// never needed — and risk linking a second, ABI-incompatible copy alongside the SDK's.
///
/// Early projects were seeded with exactly this set, so it is also the list [`ProjectConfig`]
/// drops when it loads one.
pub const SDK_PROVIDED_PORTS: &[&str] = &["glm", "imgui", "spdlog", "fmt"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendering {
    pub api: Api,
    /// Linux windowing system a Scene opens on; ignored on Windows and macOS. The runtime turns
    /// this into a GLFW init hint before `glfwInit()` — `auto` keeps GLFW's own choice (Wayland
    /// when a Wayland session is present, X11 otherwise). OpenGL always runs on X11/XWayland
    /// regardless, so a `wayland` request there is ignored by the runtime with a warning.
    ///
    /// A per-machine `--platform` override still rides in on the launch (see `builder::runtime_args`)
    /// and wins over this; this is the project's committed default, editable in the settings panel.
    #[serde(default)]
    pub platform: Platform,
    pub window: Window,
}

impl Default for Rendering {
    fn default() -> Self {
        Self { api: Api::Vulkan, platform: Platform::default(), window: Window::default() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Api {
    OpenGL,
    Vulkan,
}

/// Linux windowing system, spelled exactly as the runtime's `rendering.platform` / `--platform`
/// accepts it. Serialized lowercase (`auto` / `x11` / `wayland`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// Let GLFW pick, matching the runtime's own default.
    #[default]
    Auto,
    X11,
    Wayland,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub width: u32,
    pub height: u32,
    pub resizable: bool,
    pub fullscreen: bool,
    pub borderless: bool,
    pub transparent: bool,
    /// Defaults to on, matching the runtime's own default. Projects written before this field
    /// existed get the same behaviour they had.
    #[serde(default = "default_true")]
    pub vsync: bool,
    /// Where Dear ImGui persists its layout (window positions, docking), project-relative — the
    /// runtime resolves it against `koral.json`'s directory. A fresh project pins it to `imgui.ini`
    /// at the project root; empty means the runtime's own default, which is that same file. The Hub
    /// carries whatever is here through untouched and does not edit it in the UI: relocating the
    /// layout file is a hand edit. Skipped when empty so older projects are not churned.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub imgui_ini: String,
}

fn default_true() -> bool {
    true
}

impl Default for Window {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            resizable: true,
            fullscreen: false,
            borderless: false,
            transparent: false,
            vsync: true,
            // Empty by default so loading an older project that lacks the key does not invent one;
            // a *fresh* project pins it explicitly in `ProjectConfig::new`.
            imgui_ini: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub vcpkg_port: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub min_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// Print the exact koral.json a fresh project gets. This is the document the engine has to
    /// parse — see `ProjectConfig.ParsesADocumentInTheShapeTheHubWrites` on the engine side.
    #[test]
    fn dump_fresh_project_config() {
        let cfg = ProjectConfig::new("MyProject", "0.0.1", [0.55, 0.72, 0.61], Kind::Scene);
        println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
    }

    /// A project written before the directory lists existed must still load.
    #[test]
    fn reads_the_original_singular_path_keys() {
        let cfg: ProjectConfig = serde_json::from_str(
            r#"{ "schemaVersion":1, "name":"Old", "color":[1,0,0], "frameworkVersion":"0.0.1",
                 "paths": { "assetsDir": "content", "shadersDir": "glsl" } }"#,
        )
        .expect("old-style config should still parse");
        assert_eq!(cfg.paths.asset_directories, vec!["content".to_string()]);
        assert_eq!(cfg.paths.shader_directories, vec!["glsl".to_string()]);
    }
}
