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
    #[serde(default)]
    pub rendering: Rendering,
    /// vcpkg ports the project's own source needs. The ABI baseline is NOT stored here —
    /// it is inherited from the resolved framework SDK's manifest so Hub and framework
    /// releases can version independently.
    #[serde(default)]
    pub libraries: Vec<Library>,
}

impl ProjectConfig {
    pub const SCHEMA_VERSION: u32 = 1;

    /// A fresh project with the default rendering settings and library set.
    pub fn new(
        name: impl Into<String>,
        framework_version: impl Into<String>,
        color: [f32; 3],
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            name: name.into(),
            color,
            framework_version: framework_version.into(),
            rendering: Rendering::default(),
            libraries: default_libraries(),
        }
    }
}

/// The ports every Koral project links by default. Anything the framework itself uses
/// internally is already inside the SDK's shared library, so only what user scene code
/// directly `#include`s belongs here.
pub fn default_libraries() -> Vec<Library> {
    vec![
        Library { vcpkg_port: "glm".into(), min_version: "1.0.3".into(), features: vec![] },
        Library {
            vcpkg_port: "imgui".into(),
            min_version: "1.91.9".into(),
            features: vec!["docking-experimental".into()],
        },
        Library { vcpkg_port: "spdlog".into(), min_version: "1.15.0".into(), features: vec![] },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendering {
    pub api: Api,
    pub window: Window,
}

impl Default for Rendering {
    fn default() -> Self {
        Self { api: Api::Vulkan, window: Window::default() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Api {
    OpenGL,
    Vulkan,
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
