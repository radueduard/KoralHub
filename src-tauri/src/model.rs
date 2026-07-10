//! Portable project schema — the Koral equivalent of the old `config.gfxproj`.
//!
//! Everything in [`ProjectConfig`] is platform- and machine-independent, so a project
//! cloned from a git link resolves identically on Windows, macOS and Linux. Per-user and
//! per-machine state — the selected build profile, absolute build directories, the chosen
//! IDE — is deliberately kept OUT of this struct and lives in the Hub's local cache instead.
//! That split is what lets "clone the link and run" work across machines.
//!
//! Not wired into a command yet; this is the design target for project load/save/create.
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
