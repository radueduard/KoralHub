//! Detecting the IDEs installed on this machine and opening a project in one.
//!
//! The Hub only *opens* the folder — everything needed to build and run from inside the IDE is
//! written by [`crate::scaffold`], so an IDE launched by hand (or a project opened from the
//! IDE's own recent list) behaves identically to one launched from here.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// An IDE found on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ide {
    /// Stable key the UI sends back to `open`.
    pub id: String,
    pub name: String,
    /// The launcher actually found, shown in a tooltip so it is obvious *which* install this is.
    pub command: String,
}

/// Candidate launchers, in the order they should appear. The first command that resolves wins,
/// so a Toolbox-managed CLion and a distro-packaged one are the same entry.
const CANDIDATES: &[(&str, &str, &[&str])] = &[
    ("vscode", "VS Code", &["code", "code-insiders", "codium"]),
    ("clion", "CLion", &["clion", "clion.sh"]),
    // Windows only, and `devenv` is only on PATH inside a Developer Prompt — so this usually
    // resolves via the explicit paths below rather than the PATH lookup.
    ("vs", "Visual Studio", &["devenv"]),
];

/// Absolute fallbacks for launchers that are typically not on PATH.
#[cfg(target_os = "windows")]
const FALLBACKS: &[(&str, &str)] = &[
    (
        "vs",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\devenv.exe",
    ),
    (
        "vscode",
        r"C:\Program Files\Microsoft VS Code\Code.exe",
    ),
];
#[cfg(target_os = "macos")]
const FALLBACKS: &[(&str, &str)] = &[
    ("vscode", "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
    ("clion", "/Applications/CLion.app/Contents/MacOS/clion"),
];
#[cfg(target_os = "linux")]
const FALLBACKS: &[(&str, &str)] = &[];

/// Resolve a command through PATH, the same way a shell would.
pub fn which(command: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };

    std::env::split_paths(&path).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = dir.join(format!("{command}{ext}"));
            candidate.is_file().then(|| candidate.to_string_lossy().into_owned())
        })
    })
}

/// Every IDE this machine can open a project in.
pub fn detect() -> Vec<Ide> {
    CANDIDATES
        .iter()
        .filter_map(|(id, name, commands)| {
            let found = commands.iter().find_map(|c| which(c)).or_else(|| {
                FALLBACKS
                    .iter()
                    .filter(|(fid, _)| fid == id)
                    .map(|(_, path)| path.to_string())
                    .find(|path| Path::new(path).is_file())
            })?;
            Some(Ide {
                id: (*id).to_string(),
                name: (*name).to_string(),
                command: found,
            })
        })
        .collect()
}

/// Open `project_root` in the IDE with this id.
///
/// Detaches deliberately: the Hub must not sit holding a handle to an editor the user will keep
/// open for hours, and the IDE's own single-instance launcher usually exits immediately anyway.
pub fn open(id: &str, project_root: &Path) -> Result<(), String> {
    let ide = detect()
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("{id} is not installed on this machine"))?;

    let mut cmd = Command::new(&ide.command);
    cmd.arg(project_root);

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch {}: {e}", ide.name))
}
