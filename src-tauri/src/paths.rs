//! Per-machine locations the Hub owns. None of this travels with a project — it's the
//! local-state side of the portable/local split (installed SDKs, the recent-projects
//! index, caches).

use std::path::PathBuf;

/// Root of the Hub's local data.
/// - Linux:   `~/.local/share/KoralHub`
/// - Windows: `%LOCALAPPDATA%\KoralHub`
/// - macOS:   `~/Library/Application Support/KoralHub`
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("KoralHub")
}

/// Where downloaded framework SDKs are installed: `<data>/frameworks/<version>/<platform>/`.
pub fn frameworks_dir() -> PathBuf {
    data_dir().join("frameworks")
}

/// Thin per-machine index of project folders to show on the home screen (paths only).
pub fn recent_projects_file() -> PathBuf {
    data_dir().join("recent_projects.json")
}

/// Per-machine list of subscribed lab-collection URLs. URLs only — the manifests they point at
/// are fetched live on each open, so a course can revise its labs after students subscribe.
pub fn collections_file() -> PathBuf {
    data_dir().join("collections.json")
}

/// Per-machine index of collection repos the user is *authoring* locally (paths only), so the Hub
/// can list them for editing. Distinct from [`collections_file`], which tracks remote URLs to browse.
pub fn authored_collections_file() -> PathBuf {
    data_dir().join("authored_collections.json")
}

/// Signed-in GitHub/GitLab accounts and their OAuth tokens. **Sensitive** — the tokens grant repo
/// access, so this file is written with owner-only permissions (0600 on Unix) and never leaves the
/// machine. It is deliberately kept out of the portable project config.
pub fn accounts_file() -> PathBuf {
    data_dir().join("accounts.json")
}

/// The Hub's own preferences. Machine-local by definition — which IDE you like and where you
/// keep your projects is not something a project should carry to someone else's machine.
pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

/// Default parent folder for newly created projects (`~/Koral`).
pub fn default_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Koral")
}
