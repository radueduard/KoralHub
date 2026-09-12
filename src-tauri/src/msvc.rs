//! The MSVC build environment, for the tools the Hub spawns on Windows.
//!
//! Windows-only, and a direct consequence of building with Ninja. The Visual Studio generator finds
//! the compiler by itself — it *is* Visual Studio — but Ninja does not: it needs `cl.exe` on `PATH`,
//! along with the `INCLUDE`/`LIB` variables that make it able to find a header. Those are what a
//! "Developer Command Prompt" sets up, and a GUI app launched from Explorer or an IDE has none of
//! them. Without this, switching to Ninja would trade one platform inconsistency for a build that
//! fails on every Windows machine with "No CMAKE_CXX_COMPILER could be found".
//!
//! So the Hub does what the developer prompt does: locate the installed toolchain, run its
//! `vcvars64.bat`, and keep the environment that falls out for the children it spawns.

#[cfg(windows)]
use std::sync::RwLock;

/// The resolved environment, once we have one worth keeping.
///
/// Deliberately not a `OnceLock`. A *success* is stable and worth caching — `vcvars64.bat` costs
/// about a second and a build spawns several tools. A *failure* is not: the compiler is the one
/// dependency the user has to install themselves, so "not found" is a state they are actively
/// working to change, and the toolchain wizard's "Check again" has to be able to see that happen.
/// Caching the failure made that button a lie — the Hub would keep reporting a missing compiler for
/// the rest of the session, however much of Visual Studio had been installed in the meantime.
#[cfg(windows)]
static ENVIRONMENT: RwLock<Option<Vec<(String, String)>>> = RwLock::new(None);

/// Environment variables the build tools need in order to find MSVC, or empty when they do not.
///
/// Empty in the two cases that both mean "nothing to do": a compiler is already reachable (the Hub
/// was started from a developer prompt, or clang is on `PATH`), and no Visual Studio toolchain is
/// installed at all. In the second case the build still fails, but it fails saying no compiler was
/// found — which is the truth, and is fixed by installing one rather than by anything here.
///
/// Resolved once per run and cached: `vcvars64.bat` takes about a second, and a build spawns
/// several tools.
#[cfg(windows)]
pub fn environment() -> Vec<(String, String)> {
    if let Ok(cached) = ENVIRONMENT.read() {
        if let Some(env) = cached.as_ref() {
            return env.clone();
        }
    }

    // Already usable — don't touch an environment someone deliberately set up. Re-checked every
    // time rather than cached, since it is only a `PATH` walk.
    if crate::ide::which("cl").is_some() {
        return Vec::new();
    }

    match vcvars_environment() {
        Some(env) => {
            if let Ok(mut cached) = ENVIRONMENT.write() {
                *cached = Some(env.clone());
            }
            env
        }
        None => {
            // Not cached: the next call looks again, so installing a compiler while the Hub is
            // open is picked up by "Check again" rather than needing a restart.
            eprintln!(
                "koral-hub: no Visual Studio C++ toolchain found, so builds may fail with \
                 \"No CMAKE_CXX_COMPILER could be found\". Install \"Desktop development with \
                 C++\" (or the Build Tools), or start the Hub from a Developer Command Prompt."
            );
            Vec::new()
        }
    }
}

/// Non-Windows: nothing to set up. The compiler is on `PATH` where it belongs.
#[cfg(not(windows))]
pub fn environment() -> Vec<(String, String)> {
    Vec::new()
}

/// Run the installed toolchain's `vcvars64.bat` and capture the environment it produces.
///
/// `cmd /c call <bat> && set` is the only supported way to read these values: they are computed by
/// the batch file (from the installed toolset version, the Windows SDK present, and the host/target
/// pair), not stored anywhere they could simply be read.
#[cfg(windows)]
fn vcvars_environment() -> Option<Vec<(String, String)>> {
    let vcvars = vcvars_path()?;

    // `raw_arg`, not `arg`: Rust quotes arguments the way a C runtime expects, escaping inner
    // quotes as \" — and cmd.exe does not understand that escape, so the perfectly good command
    // line arrives mangled and the toolchain looks absent on a machine that has one. This passes
    // cmd its own syntax verbatim. `>nul` keeps the batch file's banner out of the variables.
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("cmd")
        .raw_arg(format!(r#"/c "call "{}" >nul 2>&1 && set""#, vcvars.display()))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    // `set` writes the console's ANSI code page, not UTF-8; the names and paths we care about are
    // ASCII, so a lossy read is exact for them and merely imperfect for anything else.
    let text = String::from_utf8_lossy(&output.stdout);
    let env: Vec<(String, String)> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    // A `set` that produced no PATH means the batch file did not really run.
    env.iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .then_some(env)
}

/// Locate `vcvars64.bat` in the newest Visual Studio install that has the C++ tools.
#[cfg(windows)]
fn vcvars_path() -> Option<std::path::PathBuf> {
    const RELATIVE: &str = r"VC\Auxiliary\Build\vcvars64.bat";

    // vswhere ships with every modern installer and is the supported way to find an install; it
    // knows about editions and side-by-side versions that a hard-coded path never would.
    //
    // Note the `map` rather than `?`: if `ProgramFiles(x86)` is somehow unset, that must skip
    // vswhere and fall through to the scan below, not abandon the search entirely. Returning early
    // here would report "no toolchain" on a machine that has one, purely because an environment
    // variable was missing.
    let vswhere = std::env::var_os("ProgramFiles(x86)")
        .map(|dir| std::path::PathBuf::from(dir).join(r"Microsoft Visual Studio\Installer\vswhere.exe"))
        .filter(|p| p.is_file());
    if let Some(vswhere) = vswhere {
        let output = std::process::Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                // Only an install that can actually compile — a Visual Studio with just the .NET
                // workload has no cl.exe, and pointing at it would be a slower way to fail.
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ])
            .output()
            .ok();

        if let Some(output) = output.filter(|o| o.status.success()) {
            let found = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|install| std::path::PathBuf::from(install).join(RELATIVE))
                .find(|bat| bat.is_file());
            if found.is_some() {
                return found;
            }
        }
    }

    // vswhere missing or unhelpful: fall back to scanning the standard roots, the same way the
    // bundled-ninja search does.
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Some(root) = std::env::var_os(var) else {
            continue;
        };
        let root = std::path::PathBuf::from(root).join("Microsoft Visual Studio");
        for version in newest_first(&root) {
            for edition in newest_first(&version) {
                let bat = edition.join(RELATIVE);
                if bat.is_file() {
                    return Some(bat);
                }
            }
        }
    }
    None
}

/// Subdirectories of `dir`, newest-looking first (reverse name order).
#[cfg(windows)]
fn newest_first(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    found.sort();
    found.reverse();
    found
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A failed lookup must never be remembered.
    ///
    /// The compiler is the one dependency the user installs themselves, so "not found" is a state
    /// they are in the middle of changing. If a failure were cached — as it was, in a `OnceLock` —
    /// the toolchain wizard's "Check again" would keep reporting a missing compiler for the rest of
    /// the session no matter what had been installed, and only restarting the Hub would help.
    #[test]
    fn a_failed_lookup_is_not_remembered() {
        // Whatever this machine's answer is, asking twice must give the same one, and asking again
        // must still consult the machine rather than a stored "no".
        let first = environment();
        let second = environment();
        assert_eq!(first.len(), second.len(), "repeated lookups must agree");

        let cached = ENVIRONMENT.read().unwrap().is_some();
        if first.is_empty() {
            assert!(!cached, "an empty result must never be cached — it is the one that can change");
        } else {
            assert!(cached, "a successful lookup is stable and worth keeping");
        }
    }
    /// Whatever this machine has, resolving the environment must not panic and must be
    /// self-consistent: either nothing to do, or an environment that can actually find a compiler.
    #[test]
    fn the_environment_is_empty_or_usable() {
        let env = environment();
        if env.is_empty() {
            return; // a compiler is already reachable, or none is installed
        }
        let path = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value.clone())
            .expect("an MSVC environment must carry a PATH");
        assert!(
            path.to_lowercase().contains("msvc"),
            "PATH should include the MSVC toolset directory"
        );
        assert!(
            env.iter().any(|(key, _)| key.eq_ignore_ascii_case("INCLUDE")),
            "cl.exe cannot find a header without INCLUDE"
        );
    }
}
