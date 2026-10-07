//! The Hub's own preferences: the defaults a new project starts from, and how projects are opened.
//!
//! Machine-local, and deliberately so — which IDE you use and where you keep your projects has no
//! business travelling inside a project's committed `koral.json`. This is the same portable/local
//! split that keeps `CMakePresets.json` out of git.
//!
//! Every field is *optional* in the sense that an empty value means "no preference, work it out" —
//! not "use the empty string". Resolving that fallback is [`Settings`]'s job rather than each call
//! site's, so a missing setting can never turn into an empty path or a blank IDE id.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::framework;
use crate::ide;
use crate::model::Language;
use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Parent folder new projects are created in. Empty → `~/Koral`.
    pub project_location: String,
    /// `Ide::id` of the editor to open projects with, per project language (keyed `"c++"`,
    /// `"csharp"`, `"kotlin"`) — CLion for C++ and Rider for C# is the normal setup, so one choice
    /// for everything would always be wrong for somebody. Missing or empty → the first installed
    /// IDE that can open that language.
    pub default_ides: BTreeMap<String, String>,
    /// The single preference from before it was per language. Read so an existing choice carries
    /// over (it applied to C++ projects, the only kind there was); never written back.
    #[serde(skip_serializing)]
    default_ide: String,
    /// Framework version new projects target. Empty → the newest the Hub can find.
    pub default_framework_version: String,
    /// Linux only: windowing backend a launched app should use — `"wayland"`, `"x11"`, or empty for
    /// the session default. Ignored on other platforms.
    pub display_backend: String,
}

pub fn load() -> Settings {
    // A corrupt or missing settings file must never stop the Hub from starting; defaults are
    // always a valid answer, and the user can just set them again.
    let mut s: Settings = std::fs::read_to_string(paths::settings_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let legacy = std::mem::take(&mut s.default_ide);
    if !legacy.is_empty() {
        s.default_ides.entry(Language::Cpp.tag().to_string()).or_insert(legacy);
    }
    s
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

    /// Which IDE to open a project in this language with, or `None` if this machine has none that
    /// can.
    ///
    /// A stale preference — an IDE that has since been uninstalled, or one that cannot open this
    /// language — falls back to whatever installed IDE *can* rather than failing, so the Open button
    /// keeps working.
    pub fn default_ide(&self, language: Language) -> Option<ide::Ide> {
        Self::pick(&ide::detect(), self.default_ides.get(language.tag()), language)
    }

    /// [`Self::default_ide`] for every language, detecting the installed IDEs only once.
    pub fn default_ides(&self) -> BTreeMap<String, String> {
        let installed = ide::detect();
        Language::ALL
            .iter()
            .filter_map(|&l| {
                let ide = Self::pick(&installed, self.default_ides.get(l.tag()), l)?;
                Some((l.tag().to_string(), ide.id))
            })
            .collect()
    }

    fn pick(installed: &[ide::Ide], preferred: Option<&String>, language: Language) -> Option<ide::Ide> {
        let usable = || installed.iter().filter(|i| i.supports(language));
        usable()
            .find(|i| Some(&i.id) == preferred)
            .or_else(|| usable().next())
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

