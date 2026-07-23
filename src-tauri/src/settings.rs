//! The Hub's own preferences: the defaults a new project starts from, and how projects are opened.
//!
//! Machine-local, and deliberately so — which IDE you use and where you keep your projects has no
//! business travelling inside a project's committed `koral.json`. This is the same portable/local
//! split that keeps `CMakePresets.json` out of git.
//!
//! Every field is *optional* in the sense that an empty value means "no preference, work it out" —
//! not "use the empty string". Resolving that fallback is [`Settings`]'s job rather than each call
//! site's, so a missing setting can never turn into an empty path or a blank IDE id.

use serde::{Deserialize, Serialize};

use crate::framework;
use crate::ide;
use crate::paths;

/// The framework version a project targets when nothing else says otherwise.
///
/// Only a last resort: [`Settings::framework_version`] prefers what the user picked, then the
/// newest SDK actually installed on this machine, and falls back to this only on a fresh install
/// with nothing to go on.
const FALLBACK_FRAMEWORK_VERSION: &str = "0.0.3";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Parent folder new projects are created in. Empty → `~/Koral`.
    pub project_location: String,
    /// `Ide::id` of the editor to open projects with. Empty → the first one detected.
    pub default_ide: String,
    /// Framework version new projects target. Empty → newest installed, else the fallback.
    pub default_framework_version: String,
    /// Linux only: windowing backend a launched app should use — `"wayland"`, `"x11"`, or empty for
    /// the session default. Ignored on other platforms.
    pub display_backend: String,
}

pub fn load() -> Settings {
    // A corrupt or missing settings file must never stop the Hub from starting; defaults are
    // always a valid answer, and the user can just set them again.
    std::fs::read_to_string(paths::settings_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let file = paths::settings_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&file, text).map_err(|e| format!("failed to write {}: {e}", file.display()))
}

impl Settings {
    /// Where a new project goes.
    pub fn project_location(&self) -> String {
        if self.project_location.trim().is_empty() {
            paths::default_projects_dir().to_string_lossy().into_owned()
        } else {
            self.project_location.clone()
        }
    }

    /// Which IDE to open a project with, or `None` if this machine has none installed.
    ///
    /// A stale preference — an IDE that has since been uninstalled — falls back to whatever *is*
    /// installed rather than failing, so the Open button keeps working.
    pub fn default_ide(&self) -> Option<ide::Ide> {
        let installed = ide::detect();
        installed
            .iter()
            .find(|i| i.id == self.default_ide)
            .or_else(|| installed.first())
            .cloned()
    }

    /// The framework version a new project should target.
    ///
    /// Prefers an explicit choice; otherwise the newest SDK already on this machine, so a new
    /// project builds without a download. `framework::installed` sorts newest-first.
    pub fn framework_version(&self) -> String {
        if !self.default_framework_version.trim().is_empty() {
            return self.default_framework_version.clone();
        }
        framework::installed()
            .first()
            .map(|f| f.version.clone())
            .unwrap_or_else(|| FALLBACK_FRAMEWORK_VERSION.to_string())
    }
}

