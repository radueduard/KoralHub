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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Parent folder new projects are created in. Empty → `~/Koral`.
    pub project_location: String,
    /// `Ide::id` of the editor to open projects with. Empty → the first one detected.
    pub default_ide: String,
    /// Framework version new projects target. Empty → the newest the Hub can find.
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

    /// The framework version a new project should target: the newest framework this machine can
    /// reach, or its source build.
    ///
    /// In order: an explicit choice; a build from source, because registering one is a deliberate
    /// act and a machine that has one is being used to work on the framework itself — new projects
    /// there should track that working tree rather than pin a release it is ahead of; the newest
    /// release already installed here, so the first build needs no download (`framework::installed`
    /// sorts those newest-first); and finally the newest release published for this platform, which
    /// the first build fetches on demand.
    ///
    /// `None` when this machine has no framework at all and the release list cannot be reached
    /// (offline, or rate-limited). There is no honest default to invent there, and a made-up
    /// version would only fail later, at build time, with a worse message than the caller can give
    /// now.
    pub fn framework_version(&self) -> Option<String> {
        if !self.default_framework_version.trim().is_empty() {
            return Some(self.default_framework_version.clone());
        }

        // `installed` puts source builds first, then releases newest-first, so this is "source if
        // there is one, else the newest release on disk" in a single step.
        let installed = framework::installed();
        if let Some(framework) = installed.first() {
            return Some(framework.version.clone());
        }

        // Drafts are skipped: one is unpublished by definition, and its asset 404s for anyone
        // without a token. A pre-release is offered only when it is the newest thing there is —
        // for a framework still in 0.x that may be every release it has.
        let releases = framework::available().ok()?;
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease)
            .or_else(|| releases.iter().find(|r| !r.draft))
            .map(|r| r.version.clone())
    }
}

