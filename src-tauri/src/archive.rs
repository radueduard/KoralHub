//! Export a project to a `.zip`, and import one back.
//!
//! The third way work moves between machines, alongside git (`import_project`) and a folder that is
//! already there (`import_local_project`). It is the one that needs no account, no remote and no
//! network: a student hands in a zip, an instructor hands out a starting point, a project moves onto
//! a laptop that has never been signed in to anything.
//!
//! What travels is what `koral.json` cannot regenerate: sources, assets, shaders and the config
//! itself. Build trees and the Hub's generated CMake files are left out — see [`is_excluded`].

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::project;

/// Paths, relative to the project root, that never travel in an export.
///
/// Deliberately the same set the scaffolded `.gitignore` excludes, for the same reason: every one of
/// them is either build output or is regenerated from `koral.json` on the next build. Shipping them
/// would not just bloat the zip — `CMakePresets.json` holds this machine's absolute SDK paths, so on
/// the other end it would be actively wrong.
///
/// `.git` is excluded too, which the `.gitignore` naturally does not cover. An export is a snapshot
/// of the work, not a clone of its history: history is what the git import is for, and carrying it
/// here would silently multiply the size of a zip meant to be mailed.
fn is_excluded(relative: &Path) -> bool {
    let Some(first) = relative.components().next() else {
        return false;
    };
    let first = first.as_os_str().to_string_lossy();

    // Build output, in either spelling the scaffolding produces.
    if first == "build" || first.starts_with("cmake-build-") {
        return true;
    }
    // Git history, and per-machine IDE/Hub state that names absolute paths.
    if matches!(first.as_ref(), ".git" | ".idea" | ".koral") {
        return true;
    }
    // Regenerated from koral.json on every build; CMakePresets.json is machine-specific.
    if relative.components().count() == 1
        && matches!(first.as_ref(), "CMakeLists.txt" | "CMakePresets.json" | "vcpkg.json")
    {
        return true;
    }
    // The two .vscode files that carry absolute paths. The rest of .vscode is portable and travels.
    if relative.starts_with(".vscode")
        && relative
            .file_name()
            .is_some_and(|n| n == "launch.json" || n == "c_cpp_properties.json")
    {
        return true;
    }
    false
}

/// Write `project_root` to a zip at `dest`, and return where it landed.
///
/// Entries are stored under a single top-level folder named after the project's own folder, the way
/// a GitHub source zip is: unpacking it anywhere produces one tidy directory rather than spraying
/// sources into the current one.
pub fn export(project_root: &Path, dest: &Path) -> Result<PathBuf, String> {
    // Confirm it is a project before writing anything, so a mistaken path fails with a sentence
    // rather than an empty archive named after it.
    project::load(project_root)
        .map_err(|e| format!("that folder is not a Koral project: {e}"))?;

    let top = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .ok_or("could not work out a folder name for the project")?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let file = std::fs::File::create(dest)
        .map_err(|e| format!("failed to create {}: {e}", dest.display()))?;

    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    add_dir(&mut zip, project_root, Path::new(""), &top, options)?;
    zip.finish().map_err(|e| format!("failed to finish {}: {e}", dest.display()))?;

    Ok(dest.to_path_buf())
}

/// Recursively add `dir`'s contents to the archive. `relative` is where `dir` sits inside the
/// project; `top` is the folder every entry is nested under.
fn add_dir<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Path,
    relative: &Path,
    top: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let child = relative.join(entry.file_name());
        if is_excluded(&child) {
            continue;
        }

        // Zip paths are always '/'-separated, whatever the host uses.
        let name = format!("{top}/{}", child.to_string_lossy().replace('\\', "/"));
        let file_type = entry.file_type().map_err(|e| e.to_string())?;

        if file_type.is_dir() {
            add_dir(zip, &path, &child, top, options)?;
        } else if file_type.is_file() {
            zip.start_file(&name, options)
                .map_err(|e| format!("failed to add {name}: {e}"))?;
            let mut source = std::fs::File::open(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            std::io::copy(&mut source, zip)
                .map_err(|e| format!("failed to write {name}: {e}"))?;
        }
        // Symlinks are skipped: they would either dangle on the other machine or, followed,
        // silently pull in whatever they point at from outside the project.
    }
    Ok(())
}

/// Unpack a project zip into `location` and return its root.
///
/// Accepts both the Hub's own exports and an ordinary source zip (a GitHub "Download ZIP", say), by
/// finding the `koral.json` inside rather than trusting the archive's shape. Refuses if the target
/// folder already exists, so an import never clobbers local work — the same contract as a git
/// import.
pub fn import(zip_path: &Path, location: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("failed to open {}: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("{} is not a readable zip: {e}", zip_path.display()))?;

    let prefix = project_prefix(&mut archive)?;

    // Name the folder after the archive's own project directory, falling back to the zip's name for
    // a flat archive that has none.
    let name = prefix
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            zip_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .filter(|n| !n.is_empty())
        .ok_or("could not work out a folder name from that zip")?;

    let root = location.join(&name);
    if root.exists() {
        return Err(format!(
            "a folder named '{name}' already exists in {}",
            location.display()
        ));
    }

    extract(&mut archive, &prefix, &root).inspect_err(|_| {
        // A half-written folder is worse than none: the Hub would list it as a broken project.
        let _ = std::fs::remove_dir_all(&root);
    })?;

    if let Err(e) = project::load(&root) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(format!(
            "unpacked, but it has no valid {} — it does not look like a Koral project ({e})",
            project::CONFIG_FILE
        ));
    }

    Ok(root)
}

/// The path inside the archive that the project root sits at, as a `/`-terminated prefix (empty for
/// an archive whose `koral.json` is at the top).
///
/// Chosen as the *shallowest* `koral.json`, so a project that happens to vendor another one under a
/// subdirectory still imports as itself rather than as its dependency.
fn project_prefix<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<String, String> {
    let mut best: Option<String> = None;

    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|e| format!("corrupt zip: {e}"))?;
        let Some(path) = file.enclosed_name() else {
            continue; // a path that would escape the destination is not a candidate
        };
        if path.file_name().is_some_and(|n| n == project::CONFIG_FILE) {
            let prefix = path
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
            let deeper_than = |a: &str, b: &str| a.matches('/').count() > b.matches('/').count();
            if best.as_ref().is_none_or(|b| deeper_than(b, &prefix)) {
                best = Some(prefix);
            }
        }
    }

    best.ok_or_else(|| {
        format!(
            "that zip has no {} in it — it does not look like a Koral project",
            project::CONFIG_FILE
        )
    })
}

/// Extract every entry under `prefix` into `dest`, stripping the prefix.
fn extract<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    prefix: &str,
    dest: &Path,
) -> Result<(), String> {
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("corrupt zip: {e}"))?;
        // `enclosed_name` is what stops a crafted archive writing outside `dest` ("zip slip").
        let Some(path) = file.enclosed_name() else {
            return Err(format!("that zip contains an unsafe path: {}", file.name()));
        };

        let as_str = path.to_string_lossy().replace('\\', "/");
        let Some(relative) = as_str.strip_prefix(prefix) else {
            continue; // outside the project directory in the archive
        };
        if relative.is_empty() {
            continue;
        }
        let target = dest.join(relative);

        if file.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| format!("failed to write {}: {e}", target.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|e| e.to_string())?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("koral-archive-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project survives the round trip with its sources, and comes back a project.
    #[test]
    fn a_project_round_trips_through_a_zip() {
        let dir = scratch("roundtrip");
        let root = project::create(&dir, "Lab1", "0.1.0", [0.5, 0.5, 0.5], Kind::Scene).unwrap();
        std::fs::write(root.join("assets").join("note.txt"), "hello").unwrap();

        let zip = dir.join("Lab1.zip");
        export(&root, &zip).unwrap();
        assert!(zip.is_file(), "the archive should exist");

        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let back = import(&zip, &elsewhere).unwrap();

        assert_eq!(back, elsewhere.join("Lab1"));
        assert_eq!(project::load(&back).unwrap().name, "Lab1");
        assert_eq!(
            std::fs::read_to_string(back.join("assets").join("note.txt")).unwrap(),
            "hello"
        );
        assert!(back.join("src").is_dir(), "sources should travel");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The build tree and the generated CMake files are the bulk of a project folder and are
    /// regenerated on the other end. Shipping CMakePresets.json would be worse than wasteful: it
    /// names this machine's SDK paths.
    #[test]
    fn build_output_and_generated_files_are_left_out() {
        let dir = scratch("excludes");
        let root = project::create(&dir, "Lab2", "0.1.0", [0.5, 0.5, 0.5], Kind::Scene).unwrap();
        std::fs::create_dir_all(root.join("cmake-build-debug")).unwrap();
        std::fs::write(root.join("cmake-build-debug").join("Lab2.dll"), "binary").unwrap();
        std::fs::write(root.join("CMakePresets.json"), "{}").unwrap();
        std::fs::write(root.join("CMakeLists.txt"), "# generated").unwrap();

        let zip = dir.join("Lab2.zip");
        export(&root, &zip).unwrap();

        let names: Vec<String> = {
            let mut a = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
            (0..a.len()).map(|i| a.by_index(i).unwrap().name().to_string()).collect()
        };
        assert!(names.iter().any(|n| n.ends_with("koral.json")), "config must travel: {names:?}");
        assert!(!names.iter().any(|n| n.contains("cmake-build-debug")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("CMakePresets.json")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("CMakeLists.txt")), "{names:?}");
        // Every entry sits under one top-level folder, so unpacking never sprays files about.
        assert!(names.iter().all(|n| n.starts_with("Lab2/")), "{names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Importing must never overwrite work that is already there.
    #[test]
    fn importing_over_an_existing_folder_is_refused() {
        let dir = scratch("clash");
        let root = project::create(&dir, "Lab3", "0.1.0", [0.5, 0.5, 0.5], Kind::Scene).unwrap();
        let zip = dir.join("Lab3.zip");
        export(&root, &zip).unwrap();

        // `dir` already holds Lab3 — the one just exported.
        let err = import(&zip, &dir).unwrap_err();
        assert!(err.contains("already exists"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Round-trip a real project on this machine, to see what an export actually costs and that a
    /// build tree does not ride along.
    ///
    /// Ignored: it needs a project to point at. Set `KORAL_ARCHIVE_TARGET` to one and run
    /// `cargo test --lib archive -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn round_trip_a_real_project() {
        let root = PathBuf::from(
            std::env::var("KORAL_ARCHIVE_TARGET").expect("set KORAL_ARCHIVE_TARGET"),
        );
        let dir = scratch("real");
        let zip = dir.join("export.zip");
        export(&root, &zip).expect("export should succeed");

        let on_disk: u64 = walk_size(&root);
        let archived = std::fs::metadata(&zip).unwrap().len();
        println!(
            "{}: folder {:.1} MB -> zip {:.1} MB",
            root.display(),
            on_disk as f64 / 1e6,
            archived as f64 / 1e6
        );

        let back = import(&zip, &dir).expect("import should succeed");
        let cfg = project::load(&back).expect("the copy must still be a project");
        println!("imported {} to {}", cfg.name, back.display());
        assert!(!back.join("cmake-build-debug").exists(), "no build tree should travel");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk_size(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let path = e.path();
                if path.is_dir() { walk_size(&path) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }
            })
            .sum()
    }
    /// A zip that is not a project is refused, and leaves nothing behind.
    #[test]
    fn a_zip_without_a_config_is_not_a_project() {
        let dir = scratch("notaproject");
        let zip = dir.join("random.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
            w.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"nothing to see").unwrap();
            w.finish().unwrap();
        }

        let err = import(&zip, &dir).unwrap_err();
        assert!(err.contains("does not look like a Koral project"), "{err}");
        assert!(!dir.join("random").exists(), "nothing should be left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
