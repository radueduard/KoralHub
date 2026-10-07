//! Detecting the IDEs installed on this machine and opening a project in one.
//!
//! The Hub only *opens* the project — everything needed to build and run from inside the IDE is
//! written by [`crate::scaffold`] (C++), [`crate::csharp`] and [`crate::kotlin`], so an IDE
//! launched by hand (or a project opened from the IDE's own recent list) behaves identically to
//! one launched from here.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::Language;

/// An IDE found on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ide {
    /// Stable key the UI sends back to `open`.
    pub id: String,
    pub name: String,
    /// The launcher actually found, shown in a tooltip so it is obvious *which* install this is.
    pub command: String,
    /// The project languages it can open — the UI only offers an IDE for projects it understands.
    pub languages: Vec<Language>,
}

impl Ide {
    pub fn supports(&self, language: Language) -> bool {
        self.languages.contains(&language)
    }
}

/// One IDE the Hub knows how to find.
struct Candidate {
    id: &'static str,
    name: &'static str,
    languages: &'static [Language],
    /// Launchers looked up on PATH (and in JetBrains Toolbox's scripts folder), in order.
    commands: &'static [&'static str],
    /// The executable inside a JetBrains install's `bin/`, for installs nothing put on PATH.
    jetbrains_bin: Option<&'static str>,
}

use Language::{CSharp, Cpp, Kotlin};

/// Candidate launchers, in the order they should appear. The first command that resolves wins,
/// so a Toolbox-managed CLion and a distro-packaged one are the same entry.
const CANDIDATES: &[Candidate] = &[
    Candidate {
        id: "vscode",
        name: "VS Code",
        languages: &[Cpp, CSharp, Kotlin],
        commands: &["code", "code-insiders", "codium"],
        jetbrains_bin: None,
    },
    Candidate {
        id: "clion",
        name: "CLion",
        languages: &[Cpp],
        commands: &["clion", "clion.sh"],
        jetbrains_bin: Some(if cfg!(windows) { "clion64.exe" } else { "clion.sh" }),
    },
    // C# projects natively, and CMake ones too — which is worth having for C++ because Rider lints
    // GLSL out of the box.
    Candidate {
        id: "rider",
        name: "Rider",
        languages: &[Cpp, CSharp],
        commands: &["rider", "rider.sh", "jetbrains-rider"],
        jetbrains_bin: Some(if cfg!(windows) { "rider64.exe" } else { "rider.sh" }),
    },
    Candidate {
        id: "intellij",
        name: "IntelliJ IDEA",
        languages: &[Kotlin],
        commands: &[
            "idea",
            "idea.sh",
            "intellij-idea-ultimate",
            "intellij-idea-ultimate-edition",
            "intellij-idea-community",
            "intellij-idea-community-edition",
        ],
        jetbrains_bin: Some(if cfg!(windows) { "idea64.exe" } else { "idea.sh" }),
    },
    // Windows only, and `devenv` is only on PATH inside a Developer Prompt — so this usually
    // resolves via the explicit paths below rather than the PATH lookup.
    Candidate {
        id: "vs",
        name: "Visual Studio",
        languages: &[Cpp, CSharp],
        commands: &["devenv"],
        jetbrains_bin: None,
    },
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
    ("rider", "/Applications/Rider.app/Contents/MacOS/rider"),
    ("intellij", "/Applications/IntelliJ IDEA.app/Contents/MacOS/idea"),
    ("intellij", "/Applications/IntelliJ IDEA CE.app/Contents/MacOS/idea"),
];
#[cfg(target_os = "linux")]
const FALLBACKS: &[(&str, &str)] = &[
    ("clion", "/snap/bin/clion"),
    ("rider", "/snap/bin/rider"),
    ("intellij", "/snap/bin/intellij-idea-ultimate"),
    ("intellij", "/snap/bin/intellij-idea-community"),
];

/// Resolve a command through PATH, the same way a shell would.
pub fn which(command: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| in_dir(&dir, command))
}

/// `command` in `dir`, with the platform's executable extensions.
fn in_dir(dir: &Path, command: &str) -> Option<String> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    exts.iter().find_map(|ext| {
        let candidate = dir.join(format!("{command}{ext}"));
        candidate.is_file().then(|| candidate.to_string_lossy().into_owned())
    })
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Where JetBrains Toolbox writes its shell scripts (`clion`, `rider`, `idea`) — on PATH only if
/// the user added it, which most never do.
fn toolbox_scripts() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"JetBrains\Toolbox\scripts"))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support/JetBrains/Toolbox/scripts"))
    } else {
        home().map(|h| h.join(".local/share/JetBrains/Toolbox/scripts"))
    }
}

/// Folders a JetBrains IDE is installed *under* without anything on PATH: Toolbox's apps folder,
/// the standalone installers' `Program Files\JetBrains`, a tarball unpacked into `/opt`.
fn jetbrains_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "LOCALAPPDATA"] {
            if let Some(d) = std::env::var_os(var) {
                let d = PathBuf::from(d);
                roots.push(if var == "ProgramFiles" { d.join("JetBrains") } else { d.join("Programs") });
            }
        }
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(d).join(r"JetBrains\Toolbox\apps"));
        }
    } else if cfg!(target_os = "linux") {
        if let Some(h) = home() {
            roots.push(h.join(".local/share/JetBrains/Toolbox/apps"));
        }
        roots.push(PathBuf::from("/opt"));
    }
    roots
}

/// The newest `bin/<exe>` anywhere a few levels under `root`. Toolbox nests installs as
/// `apps/<product>/ch-0/<build>/bin` (older) or `apps/<product>/bin` (newer); the installers use
/// `JetBrains/<Product> <version>/bin`. Version folders sort, so the last match is the newest.
fn find_jetbrains_bin(root: &Path, exe: &str, depth: u32) -> Option<PathBuf> {
    let candidate = root.join("bin").join(exe);
    if candidate.is_file() {
        return Some(candidate);
    }
    if depth == 0 {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.iter().rev().find_map(|d| find_jetbrains_bin(d, exe, depth - 1))
}

fn locate(c: &Candidate) -> Option<String> {
    if let Some(found) = c.commands.iter().find_map(|cmd| which(cmd)) {
        return Some(found);
    }
    if c.jetbrains_bin.is_some() {
        if let Some(found) = toolbox_scripts().and_then(|dir| c.commands.iter().find_map(|cmd| in_dir(&dir, cmd))) {
            return Some(found);
        }
    }
    if let Some(found) = FALLBACKS
        .iter()
        .filter(|(id, _)| *id == c.id)
        .map(|(_, path)| path.to_string())
        .find(|path| Path::new(path).is_file())
    {
        return Some(found);
    }
    let exe = c.jetbrains_bin?;
    // Scoped to folders named for this product, so `idea.sh` is never found inside some other
    // JetBrains IDE's install, and `/opt` is not walked wholesale.
    let product = c.id.to_lowercase();
    jetbrains_roots().iter().find_map(|root| {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                p.is_dir() && (name.contains(&product) || (product == "intellij" && name.starts_with("idea")))
            })
            .collect();
        dirs.sort();
        dirs.iter()
            .rev()
            .find_map(|d| find_jetbrains_bin(d, exe, 3))
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// Every IDE this machine can open a project in.
pub fn detect() -> Vec<Ide> {
    CANDIDATES
        .iter()
        .filter_map(|c| {
            Some(Ide {
                id: c.id.to_string(),
                name: c.name.to_string(),
                command: locate(c)?,
                languages: c.languages.to_vec(),
            })
        })
        .collect()
}

/// What to hand the IDE: the project folder, except for a C# project in a .NET IDE, which wants
/// the `.csproj` — given a bare folder, Rider and Visual Studio open it as loose files with no
/// project model, and so no completion against `Koral.dll`.
fn target(ide: &Ide, language: Language, project_root: &Path, name: &str) -> PathBuf {
    if language == Language::CSharp && matches!(ide.id.as_str(), "rider" | "vs") {
        let csproj = project_root.join(format!("{name}.csproj"));
        if csproj.is_file() {
            return csproj;
        }
    }
    project_root.to_path_buf()
}

/// Open `project_root` in the IDE with this id.
///
/// Detaches deliberately: the Hub must not sit holding a handle to an editor the user will keep
/// open for hours, and the IDE's own single-instance launcher usually exits immediately anyway.
pub fn open(id: &str, language: Language, project_root: &Path, name: &str) -> Result<(), String> {
    let ide = detect()
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("{id} is not installed on this machine"))?;
    if !ide.supports(language) {
        return Err(format!("{} cannot open a {} project", ide.name, language.label()));
    }

    // Strip the Hub's inherited loader environment (an AppImage's bundled-lib paths) so the IDE —
    // and anything it in turn launches, like the same cmake — runs against the system libraries.
    let mut cmd = crate::builder::external_command(&ide.command);
    cmd.arg(target(&ide, language, project_root, name));

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch {}: {e}", ide.name))
}
