//! Thin `#[tauri::command]` layer exposed to the SolidJS frontend. Real work lives in the
//! `project` and `framework` modules; these adapt to/from UI-shaped DTOs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::archive;
use crate::toolchain;
use crate::auth::{self, AccountView, DeviceLogin, Provider};
use crate::builder;
use crate::collection;
use crate::framework::{self, AvailableFramework, InstalledFramework};
use crate::git::{self, GitInfo};
use crate::ide;
use crate::model::{Kind, ProjectConfig};
use crate::modules;
use crate::project;
use crate::scaffold;
use crate::settings::{self, Settings};
use crate::vcpkg;

/// A recent-projects list item sent to the UI. `path` is machine-local (from the recent
/// index); the rest is read from each project's committed, portable Koral config.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    pub name: String,
    pub path: String,
    /// Accent color, linear RGB in [0, 1].
    pub color: [f32; 3],
    pub framework_version: String,
    /// Shown on the card, and decides whether the settings panel offers window options.
    pub kind: Kind,
    /// Git status for the card (branch / dirty / remote), or `None` if the folder isn't a repo.
    pub git: Option<GitInfo>,
}

impl RecentProject {
    /// Read a project folder into its list entry, or an error saying why it is not one.
    fn load(root: &Path) -> Result<Self, String> {
        let cfg = project::load(root)?;
        Ok(RecentProject {
            name: cfg.name,
            git: git::info(root),
            path: root.to_string_lossy().into_owned(),
            color: cfg.color,
            framework_version: cfg.framework_version,
            kind: cfg.kind,
        })
    }
}

/// The projects to show on the home screen: recent paths whose `koral.json` still loads.
#[tauri::command]
pub fn list_recent_projects() -> Vec<RecentProject> {
    project::recent_paths()
        .into_iter()
        .filter_map(|path| RecentProject::load(&path).ok())
        .collect()
}

/// One project folder's entry, for a project the recent list does not carry.
///
/// A collection's entries are its own submodule checkouts, which are real Koral projects the
/// sidebar offers but that nobody ever added to the recent list. Without this they would be
/// selectable and then have nothing to show.
#[tauri::command]
pub fn project_details(path: String) -> Result<RecentProject, String> {
    RecentProject::load(Path::new(&path))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectRequest {
    /// Parent folder; the project is created in `location/name`.
    pub location: String,
    pub name: String,
    /// Framework release to target; defaults to the latest known if omitted.
    #[serde(default)]
    pub framework_version: Option<String>,
    /// Scene (realtime, windowed) or Job (single dispatch, headless). Defaults to Scene.
    #[serde(default)]
    pub kind: Kind,
}

/// Scaffold a new project, add it to the recent index, and return its list entry.
#[tauri::command]
pub fn create_project(req: CreateProjectRequest) -> Result<RecentProject, String> {
    let version = match req.framework_version {
        Some(v) => v,
        // Refusing here beats scaffolding a project pinned to a version that does not exist: the
        // failure is the same either way, but this one names what to do about it.
        None => settings::load().framework_version().ok_or_else(|| {
            "No framework to target: none is installed here, and the published releases could not \
             be reached. If you have built Koral yourself, register your install prefix under \
             Frameworks → Add source build — that needs no network. Otherwise install a release."
                .to_string()
        })?,
    };
    let color = project::random_color();

    let root = project::create(Path::new(&req.location), &req.name, &version, color, req.kind)?;
    project::add_recent(&root)?;

    Ok(RecentProject {
        name: req.name,
        git: git::info(&root),
        path: root.to_string_lossy().into_owned(),
        color,
        framework_version: version,
        kind: req.kind,
    })
}

/// Import an existing Koral project by cloning a git repository, then adding it to the recent list.
///
/// Clones into `location/<repo-name>` and refuses if that folder already exists, so an import never
/// clobbers local work. A clone that turns out not to be a Koral project is deleted again rather
/// than left as a stray folder the Hub would then show as broken.
///
/// Blocking: it runs on Tauri's command thread, not the UI thread, so the front end just shows a
/// pending state while it works.
#[tauri::command]
pub fn import_project(req: ImportProjectRequest) -> Result<RecentProject, String> {
    let url = req.url.trim();
    if url.is_empty() {
        return Err("enter a git URL to import".into());
    }
    let name = git::repo_name_from_url(url);
    if name.is_empty() {
        return Err("could not work out a project name from that URL".into());
    }

    let root = Path::new(&req.location).join(&name);
    if root.exists() {
        return Err(format!(
            "a folder named '{name}' already exists in {}",
            req.location
        ));
    }

    git::clone(url, &root)?;

    if let Err(e) = project::load(&root) {
        // Not a Koral project — don't leave the clone lying around.
        let _ = std::fs::remove_dir_all(&root);
        return Err(format!(
            "cloned, but it has no valid {} — it does not look like a Koral project ({e})",
            project::CONFIG_FILE
        ));
    }
    project::add_recent(&root)?;

    RecentProject::load(&root)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectRequest {
    /// HTTPS git URL to clone.
    pub url: String,
    /// Parent folder; the project lands in `location/<repo-name>`.
    pub location: String,
}

/// Add a Koral project that is already on this machine to the list, leaving it exactly where it is.
///
/// The counterpart to [`import_project`]: a project does not have to come from a remote to be one.
/// A folder cloned by hand, restored from a backup, copied off a memory stick or written before the
/// Hub existed is a project the moment it has a `koral.json`, and the only thing standing between
/// it and the sidebar was a git URL it may never have had.
///
/// Nothing is copied, moved or initialised — the folder is recorded in the recent index as-is, so
/// this is safe to point at work in progress. It needs no network and cannot fail part way.
#[tauri::command]
pub fn import_local_project(path: String) -> Result<RecentProject, String> {
    let root = Path::new(path.trim());
    if path.trim().is_empty() {
        return Err("choose a project folder to import".into());
    }
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }

    // Canonicalised so the same project reached by two paths (a symlink, a relative path, a
    // trailing slash) is one entry rather than a duplicate row pointing at the same files —
    // and through paths::canonicalize, which strips the Windows `\\?\` prefix that would
    // otherwise make the plain and verbatim forms of one project two rows in the recent index.
    let root = crate::paths::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;

    project::load(&root).map_err(|e| {
        format!(
            "that folder has no valid {} — it does not look like a Koral project ({e})",
            project::CONFIG_FILE
        )
    })?;

    project::add_recent(&root)?;
    RecentProject::load(&root)
}

/// Write a project to a zip at `dest`, and return where it landed.
///
/// The offline way to hand work over: no account, no remote, no network. Build trees and the
/// Hub's generated CMake files are left out — they are rebuilt from `koral.json` on the other
/// end, and `CMakePresets.json` names this machine's SDK paths, so sending it would be wrong
/// rather than merely wasteful. See `archive::is_excluded`.
///
/// Blocking: it walks and compresses the tree, so the UI shows a pending state, like an import.
#[tauri::command]
pub fn export_project(path: String, dest: String) -> Result<String, String> {
    let dest = dest.trim();
    if dest.is_empty() {
        return Err("choose where to save the zip".into());
    }
    archive::export(Path::new(&path), Path::new(dest))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Unpack a project zip into `location` and add it to the recent list.
///
/// The counterpart to [`export_project`], and the third way in alongside [`import_project`] (git)
/// and [`import_local_project`] (a folder already here). Refuses if the target folder exists, so
/// an import never clobbers local work.
///
/// Blocking: it decompresses on the command thread while the UI shows a pending state.
#[tauri::command]
pub fn import_project_zip(path: String, location: String) -> Result<RecentProject, String> {
    let root = archive::import(Path::new(path.trim()), Path::new(location.trim()))?;
    project::add_recent(&root)?;
    RecentProject::load(&root)
}

/// Bring a project up to date with its git remote, **overwriting local work**.
///
/// There is no merge: `origin` wins outright, so local commits and edits to tracked files are gone.
/// That is the point — this is for a lab or a shared project whose current version is upstream's,
/// where an update that could stop half way through a conflict would be no use at all. The UI
/// confirms it before calling, since it is not undoable.
///
/// Blocking: it fetches, so the UI shows a pending state, like an import.
#[tauri::command]
pub fn update_project_from_git(path: String) -> Result<git::UpdateReport, String> {
    let root = Path::new(&path);
    project::load(root).map_err(|e| format!("that folder is not a Koral project: {e}"))?;

    let report = git::update_from_origin(root)?;
    // A project can carry its modules as submodules; leaving them behind would update the sources
    // and not what they depend on.
    if let Err(e) = git::update_submodules(root) {
        eprintln!("koral-hub: submodule update failed for {}: {e}", root.display());
    }

    // The overwrite replaced koral.json along with everything else, so confirm what landed is
    // still a project rather than leaving a card that silently stops loading.
    project::load(root).map_err(|e| {
        format!("updated to {}, but the result is not a valid Koral project: {e}", report.to)
    })?;
    Ok(report)
}

/// Bring an authored collection up to date with its git remote, **overwriting local work** — the
/// manifest, and every entry it checks out.
///
/// Same contract as [`update_project_from_git`], applied to the collection repository and then to
/// each submodule, so entries move to the commits the refreshed manifest records.
#[tauri::command]
pub fn update_collection_from_git(path: String) -> Result<git::UpdateReport, String> {
    let root = Path::new(&path);
    collection::load_manifest(root).map_err(|e| format!("that folder is not a collection: {e}"))?;

    let report = git::update_from_origin(root)?;
    git::update_submodules(root)?;

    collection::load_manifest(root).map_err(|e| {
        format!("updated to {}, but the result is not a valid collection: {e}", report.to)
    })?;
    Ok(report)
}

/// Remove a project from the recent list and, if asked, delete its folder from disk.
///
/// `delete_files` is irreversible — the UI must confirm it, and it defaults to off.
#[tauri::command]
pub fn remove_project(path: String, delete_files: bool) -> Result<(), String> {
    project::delete(Path::new(&path), delete_files)
}

/// A project's full `koral.json`, for the settings panel.
///
/// Libraries that carry no CMake names get them filled in from this machine's port tree on the way
/// out — a project written before those fields existed otherwise shows two blank boxes and gives
/// the user nothing to check. This only *offers* the values; they reach the file when the panel is
/// saved, and the build resolves them the same way regardless (see `scaffold::resolve_library`).
#[tauri::command]
pub fn project_config(path: String) -> Result<ProjectConfig, String> {
    let mut cfg = project::load(Path::new(&path))?;
    for library in &mut cfg.libraries {
        if library.packages.is_empty() {
            if let Some((packages, targets)) = vcpkg::cmake_names(&library.vcpkg_port) {
                library.packages = packages;
                library.targets = targets;
            }
        }
    }
    Ok(cfg)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddLocalFrameworkRequest {
    /// The install prefix — the directory `cmake --install --prefix` wrote to.
    pub path: String,
    /// The framework's source tree, when the user picked one by hand. Empty means "work it out
    /// from the build's CMake cache", which is what the dialog sends unless they overrode it.
    #[serde(default)]
    pub source_path: String,
}

/// Register a framework built from source by path, so projects can target a framework that was
/// never released — a build from source, or a release candidate being tested.
///
/// It gets **no version**: a source tree changes under you, so a number stamped on it would be
/// stale the moment it was rebuilt. Projects target it by kind instead (`"frameworkVersion":
/// "source"`), which resolves fresh on every build.
///
/// The directory is validated (it must hold a runtime and a `*Config.cmake`) but never copied or
/// modified: the Hub only remembers where it is, so a rebuild is picked up with no re-registration.
#[tauri::command]
pub fn add_local_framework(
    req: AddLocalFrameworkRequest,
) -> Result<framework::local::LocalFramework, String> {
    framework::local::add(Path::new(&req.path), &req.source_path)
}

/// Forget a registered source build, by name. The directory itself is left alone — it is the
/// user's build output, which the Hub has no business deleting.
#[tauri::command]
pub fn remove_local_framework(name: String) -> Result<(), String> {
    framework::local::remove(&name)
}

/// Point a registered source build at its source tree, for when the Hub could not work it out —
/// a build directory that has since been cleaned, or a prefix copied away from its build.
///
/// This is what a debugger searches to show framework code, so it is worth being able to fix.
#[tauri::command]
pub fn set_framework_source(name: String, source_path: String) -> Result<(), String> {
    framework::local::set_source_dir(&name, &source_path)
}

/// What the Hub can work out about an install prefix before it is registered: the source tree it
/// was built from, and that build's type. Lets the Add dialog show what it found — and, when it
/// found nothing, ask for the source folder instead of silently registering a build that cannot
/// be stepped into.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedSource {
    pub source_path: String,
    pub build_path: String,
    pub build_type: String,
}

#[tauri::command]
pub fn detect_framework_source(path: String) -> DetectedSource {
    let (source_path, build_path) = framework::local::discover_source(Path::new(&path));
    DetectedSource {
        build_type: framework::local::build_type(&build_path).unwrap_or_default(),
        source_path,
        build_path,
    }
}

/// The modules this machine can offer a project targeting `framework_version`: the user's own
/// module projects, plus the ones inside that installed SDK. Feeds the settings panel's module
/// picker, so a project's `"modules"` list is chosen rather than typed.
///
/// Purely local — it lists what is already here and never reaches the network, since it runs every
/// time the settings panel opens. A framework that is not installed yet simply contributes no
/// modules of its own.
#[tauri::command]
pub fn available_modules(framework_version: String) -> Vec<modules::ModuleView> {
    modules::list_available(&framework_version)
}

/// One entry of a collection as the UI sees it: what the manifest says, plus every copy of it that
/// exists on this machine.
///
/// The first path — when there is one — is what makes an entry selectable in the sidebar alongside
/// ordinary projects: with it the entry *is* a project (build it, run it, edit its settings);
/// with none, it is something to download.
///
/// The *rest* matter just as much, because the sidebar hides a project that a collection already
/// accounts for. There can genuinely be several: the project you authored and added, plus a copy
/// you later downloaded from your own collection — a repository you own keeps its `origin`, so
/// both are the same entry. Reporting only one would leave the other showing a second time, as if
/// it were an unrelated project.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LabView {
    #[serde(flatten)]
    pub lab: collection::Lab,
    pub local_paths: Vec<String>,
}

/// Compare two git URLs for "the same repository", across the https/ssh and `.git`/bare spellings
/// of it. Deliberately conservative: it only ever reports a match it can justify, since the
/// consequence of a false positive is a sidebar entry pointing at the wrong folder.
fn same_repo(a: &str, b: &str) -> bool {
    fn key(url: &str) -> String {
        let mut rest = url.trim();
        if let Some(i) = rest.find("://") {
            rest = &rest[i + 3..];
        }
        if let Some(i) = rest.rfind('@') {
            rest = &rest[i + 1..];
        }
        rest.trim_end_matches('/')
            .trim_end_matches(".git")
            .replace(':', "/")
            .to_ascii_lowercase()
    }
    let (a, b) = (key(a), key(b));
    !a.is_empty() && a == b
}

/// What is already on this machine, read once so that resolving a collection's entries costs one
/// pass over these lists rather than one git-repo open *per entry*. A course collection has dozens
/// of entries and the user has dozens of projects; without this, listing collections is a few
/// thousand repository opens.
struct LocalIndex {
    /// Recent projects and the repository each came from — its `origin` when it still has one,
    /// otherwise the source URL the Hub recorded when it downloaded it (see
    /// `project::source_url`). A lab from someone else's repo has no `origin` by design, so
    /// without the fallback every downloaded lab would look unrelated to its collection.
    projects: Vec<(PathBuf, Option<String>)>,
    /// Authored collections and the remote each publishes to.
    authored: Vec<(String, String)>,
}

impl LocalIndex {
    fn load() -> Self {
        LocalIndex {
            projects: project::recent_paths()
                .into_iter()
                .map(|path| {
                    let origin = git::origin_url(&path)
                        .or_else(|| project::source_url(&path));
                    (path, origin)
                })
                .collect(),
            authored: collection::authored_paths()
                .into_iter()
                .filter_map(|root| {
                    let origin = git::origin_url(&root)?;
                    Some((root.to_string_lossy().into_owned(), origin))
                })
                .collect(),
        }
    }

    /// The locally-authored collection published at this URL, if any.
    ///
    /// Subscribing to your own published collection is an easy thing to do — you paste the link to
    /// check it works — and leaves the Hub holding two records of one collection. This is what lets
    /// the UI show it once, as the copy you can actually edit.
    fn authored_at(&self, url: &str) -> Option<String> {
        self.authored
            .iter()
            .find(|(_, origin)| same_repo(origin, url))
            .map(|(path, _)| path.clone())
    }

    /// Every copy of a collection entry on this machine, the one to work in first.
    ///
    /// Projects on the recent list whose `origin` is this repository lead — those are the copies
    /// the user actually opens — followed by the collection's own submodule checkout, which is
    /// what an author has for entries they never cloned separately. All of them are returned, not
    /// just the best one, so the sidebar can hide every copy rather than leaving the runners-up
    /// looking like separate projects.
    fn local_paths_for(&self, url: &str, checkout: Option<PathBuf>) -> Vec<String> {
        let mut paths: Vec<String> = self
            .projects
            .iter()
            .filter(|(_, origin)| origin.as_deref().is_some_and(|o| same_repo(o, url)))
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect();

        if let Some(dir) = checkout.filter(|dir| project::load(dir).is_ok()) {
            let dir = dir.to_string_lossy().into_owned();
            // The checkout can also be on the recent list, if it was imported from there.
            if !paths.contains(&dir) {
                paths.push(dir);
            }
        }
        paths
    }
}

/// A subscribed lab collection as shown in the UI: its source URL, plus *either* the fetched
/// manifest or the error fetching it produced. Carrying the error per-collection is deliberate —
/// one unreachable or malformed collection must not blank out the others.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionView {
    pub url: String,
    pub title: String,
    pub description: String,
    pub contents: collection::Contents,
    pub labs: Vec<LabView>,
    pub error: Option<String>,
    /// Set when these contents came from the offline cache because the fetch failed, carrying why.
    ///
    /// Distinct from `error`, and never set at the same time: `error` means there is nothing to
    /// show, while this means “this is what it looked like last time”. The entries are real and
    /// still worth working from — a lab already downloaded opens and builds with no network at
    /// all — they just may not be current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    /// Set when this subscription is to a collection the user authors locally — the same
    /// collection reached by its published URL. The UI shows the authored copy instead, since it
    /// can do everything this one can and more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored_path: Option<String>,
}

impl CollectionView {
    fn fetched(url: String, index: &LocalIndex) -> Self {
        let authored_path = index.authored_at(&url);
        // Cache-backed, so a collection stays browsable with no network. `add_collection` still
        // validates with a bare `fetch` — a cache must never be what puts a collection on the list.
        match collection::fetch_or_cached(&url) {
            Ok(collection::Fetched { manifest, stale }) => CollectionView {
                stale,
                authored_path,
                title: manifest.title,
                description: manifest.description,
                contents: manifest.contents,
                labs: manifest
                    .labs
                    .into_iter()
                    .map(|lab| LabView {
                        // A subscribed collection has no checkout of its own — its entries are
                        // either downloaded as projects or not here at all.
                        local_paths: index.local_paths_for(&lab.url, None),
                        lab,
                    })
                    .collect(),
                url,
                error: None,
            },
            Err(e) => CollectionView {
                authored_path,
                stale: None,
                title: String::new(),
                description: String::new(),
                contents: collection::Contents::default(),
                labs: Vec::new(),
                url,
                error: Some(e),
            },
        }
    }
}

/// Every collection the user has added, each re-fetched now. Hits the network once per collection;
/// a slow or offline one falls back to its cached contents (marked `stale`), and only a collection
/// with no cache at all surfaces as that collection's `error` — never as a failed command.
///
/// The fetches run concurrently, which matters most in the case that motivates the cache. Serially,
/// a machine with no network pays the connect timeout once per collection before any of them can
/// be drawn; in parallel the whole list costs one timeout. `LocalIndex` is read-only here and
/// shared by reference, which is what `scope` exists to make safe.
#[tauri::command]
pub fn list_collections() -> Vec<CollectionView> {
    let index = LocalIndex::load();
    let urls = collection::subscribed_urls();

    // A shared reference, so each closure moves its own `url` but only borrows the one index.
    let index = &index;
    std::thread::scope(|scope| {
        let handles: Vec<_> = urls
            .into_iter()
            .map(|url| scope.spawn(move || CollectionView::fetched(url, index)))
            .collect();
        handles
            .into_iter()
            // A panicked fetch must not take the whole list with it: drop that one collection
            // and still show the rest, exactly as an unreachable one does.
            .filter_map(|h| h.join().ok())
            .collect()
    })
}

/// Add a collection by URL and return its freshly-fetched view.
///
/// Validates by fetching *before* remembering it: a URL that yields no manifest is not a
/// collection, and persisting it would only add a permanently-broken row to the list.
#[tauri::command]
pub fn add_collection(url: String) -> Result<CollectionView, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("enter a collection URL".into());
    }
    // Validated by fetching before it is remembered; `fetched` then does it once more to build the
    // view, which also gives the entries their local links.
    collection::fetch(&url)?;
    collection::add(&url)?;
    Ok(CollectionView::fetched(url, &LocalIndex::load()))
}

/// Forget a collection. Labs already downloaded from it are untouched — they are ordinary projects
/// now, with no link back to the collection they came from.
#[tauri::command]
pub fn remove_collection(url: String) -> Result<(), String> {
    collection::remove(&url)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadLabRequest {
    /// HTTPS git URL of the lab's repository.
    pub url: String,
    /// The user's projects folder. The lab lands under the collection's own folder inside it —
    /// see [`collection::downloads_dir`] — as a fresh project.
    pub location: String,
    /// URL of the collection this lab is being taken from, which names the folder its downloads
    /// share. Empty (the default) keeps the old flat layout, so a caller that does not know or
    /// care which collection an entry came from still works.
    #[serde(default)]
    pub collection: String,
}

/// Download a lab into `location` as a fresh project, and add it to the recent list.
///
/// Blocking: clones on the command thread while the UI shows a pending state, exactly like
/// [`import_project`]. The difference is the result — a lab download drops the upstream history so
/// the copy is the student's own (see `collection::download_lab`).
#[tauri::command]
pub fn download_lab(req: DownloadLabRequest) -> Result<RecentProject, String> {
    let location = collection::downloads_dir(Path::new(&req.location), &req.collection)?;
    let (root, _cfg) = collection::download_lab(&req.url, &location)?;
    RecentProject::load(&root)
}

/// A collection the user is authoring locally, for the "My Collections" list. `path` is machine-
/// local; the rest is read from the collection's own committed `koral-collection.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoredCollection {
    pub path: String,
    pub title: String,
    pub description: String,
    /// What this collection holds — decides which pickers the UI offers when adding to it.
    pub contents: collection::Contents,
    pub lab_count: usize,
    /// The entries in the collection, in order — so the sidebar can expand it, and the detail
    /// panel can reorder and remove them.
    pub labs: Vec<LabView>,
    /// Git status for the card (branch / dirty / remote) — a collection is always a repo.
    pub git: Option<GitInfo>,
}

impl AuthoredCollection {
    /// Build the view for a collection on disk, or `None` if its manifest no longer loads.
    fn load(root: &Path, index: &LocalIndex) -> Option<Self> {
        let manifest = collection::load_manifest(root).ok()?;
        Some(AuthoredCollection {
            path: root.to_string_lossy().into_owned(),
            title: manifest.title,
            description: manifest.description,
            contents: manifest.contents,
            lab_count: manifest.labs.len(),
            labs: manifest
                .labs
                .into_iter()
                .map(|lab| LabView {
                    // Each entry is a submodule, checked out into the collection under the folder
                    // its URL names — the same derivation `collection::add_lab` used.
                    local_paths: index
                        .local_paths_for(&lab.url, Some(root.join(git::repo_name_from_url(&lab.url)))),
                    lab,
                })
                .collect(),
            git: git::info(root),
        })
    }
}

/// Collections the user is building locally (those whose manifest still loads).
#[tauri::command]
pub fn list_authored_collections() -> Vec<AuthoredCollection> {
    let index = LocalIndex::load();
    collection::authored_paths()
        .iter()
        .filter_map(|p| AuthoredCollection::load(p, &index))
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCollectionRequest {
    /// Parent folder; the collection is created in `location/name`.
    pub location: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What the collection holds: projects, modules, or both. Fixed at creation.
    #[serde(default)]
    pub contents: collection::Contents,
}

/// Scaffold a new collection repo, record it in the authored index, and return its list entry.
#[tauri::command]
pub fn create_collection(req: CreateCollectionRequest) -> Result<AuthoredCollection, String> {
    let root =
        collection::create(Path::new(&req.location), &req.name, &req.description, req.contents)?;
    collection::add_authored(&root)?;
    AuthoredCollection::load(&root, &LocalIndex::load())
        .ok_or_else(|| "created the collection but could not read it back".into())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddLabRequest {
    /// The authored collection to add to.
    pub path: String,
    /// HTTPS git URL of the lab's repository, added as a submodule.
    pub url: String,
    /// Optional display name; defaults to the lab's repo name.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Add a lab to a local collection as a git submodule and return the updated list entry.
///
/// Blocking: the submodule clone reaches out to the remote, so the UI shows a pending state while
/// it works, just like an import.
#[tauri::command]
pub fn add_lab_to_collection(req: AddLabRequest) -> Result<AuthoredCollection, String> {
    let root = Path::new(&req.path);
    let name = (!req.name.trim().is_empty()).then(|| req.name.as_str());
    collection::add_lab(root, &req.url, name, &req.description)?;
    AuthoredCollection::load(root, &LocalIndex::load())
        .ok_or_else(|| "added the lab but could not read the collection back".into())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddProjectRequest {
    /// The authored collection to add to.
    pub path: String,
    /// An existing local Koral project to add as a submodule.
    pub project_path: String,
    /// Optional description shown in the collection; the display name is the project's own.
    #[serde(default)]
    pub description: String,
    /// Auto-publish parameters, used only when the project has no `origin` yet. `host` selects the
    /// signed-in account to create the repo under; `repo_name` defaults to the project's name.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub repo_name: String,
    #[serde(default)]
    pub private: bool,
}

/// Add an existing local project to a collection as a git submodule and return the updated entry.
///
/// A submodule tracks a URL, so a project that has never been pushed is published first: a remote
/// repo is created under the chosen account, wired up as `origin` and pushed — then that URL is what
/// the submodule follows. A project that already has an `origin` is added straight from it.
///
/// Blocking: publishing and the submodule clone are network round trips, so the UI shows a pending
/// state, just like an import.
#[tauri::command]
pub fn add_project_to_collection(req: AddProjectRequest) -> Result<AuthoredCollection, String> {
    let collection_root = Path::new(&req.path);
    let project_root = Path::new(&req.project_path);

    // Confirm it's a Koral project (and get its name for the display default) before touching git.
    let cfg = project::load(project_root)
        .map_err(|e| format!("that folder is not a Koral project: {e}"))?;

    // The contents rule is enforced again inside add_lab, but checking here — before anything is
    // published or cloned — turns "create a remote repo, push, clone, then refuse" into a plain
    // refusal with nothing to undo.
    let manifest = collection::load_manifest(collection_root)?;
    if !manifest.contents.accepts(Some(cfg.kind)) {
        let what = match cfg.kind {
            Kind::Module => "a module",
            _ => "a project",
        };
        return Err(format!(
            "'{}' is {what}, but this collection holds {} only",
            cfg.name,
            manifest.contents.describe()
        ));
    }

    // A submodule needs a URL. Use the project's existing origin, or publish it to obtain one.
    let url = match git::origin_url(project_root) {
        Some(url) => url,
        None => {
            let host = req.host.trim();
            let account = auth::account_for_host(host)
                .ok_or_else(|| format!("not signed in to {host} — sign in first"))?;
            // A Hub-made project is already a repo; a hand-made one might not be, and there'd be
            // nothing to push. Ensure a repo (with an initial commit) exists first.
            if git::info(project_root).is_none() {
                git::init(project_root)?;
            }
            let repo_name = req.repo_name.trim();
            let repo_name = if repo_name.is_empty() { cfg.name.trim() } else { repo_name };
            let url = auth::create_remote_repo(&account, repo_name, "", req.private)?;
            git::set_remote(project_root, "origin", &url)?;
            git::push(project_root)?;
            url
        }
    };

    let name = cfg.name.trim();
    let name = (!name.is_empty()).then_some(name);
    collection::add_lab(collection_root, &url, name, &req.description)?;
    AuthoredCollection::load(collection_root, &LocalIndex::load())
        .ok_or_else(|| "added the project but could not read the collection back".into())
}

/// Remove one project from a collection (drop its submodule + manifest entry) and return the updated
/// entry. Identified by URL, as the manifest stores it.
#[tauri::command]
pub fn remove_lab_from_collection(path: String, url: String) -> Result<AuthoredCollection, String> {
    let root = Path::new(&path);
    collection::remove_lab(root, &url)?;
    AuthoredCollection::load(root, &LocalIndex::load())
        .ok_or_else(|| "removed the project but could not read the collection back".into())
}

/// Move a project one place earlier (`up`) or later within a collection, and return the updated
/// entry. Reordering is presentational only — it never re-clones a submodule.
#[tauri::command]
pub fn reorder_lab_in_collection(
    path: String,
    url: String,
    up: bool,
) -> Result<AuthoredCollection, String> {
    let root = Path::new(&path);
    collection::reorder_lab(root, &url, up)?;
    AuthoredCollection::load(root, &LocalIndex::load())
        .ok_or_else(|| "reordered but could not read the collection back".into())
}

/// Remove an authored collection from the list and, if asked, delete its folder from disk.
///
/// `delete_files` is irreversible — the UI must confirm it, and it defaults to off. Mirrors
/// [`remove_project`].
#[tauri::command]
pub fn remove_authored_collection(path: String, delete_files: bool) -> Result<(), String> {
    collection::delete_authored(Path::new(&path), delete_files)
}

/// Emitted as `device-login-finished` when a sign-in ends, successfully or not. The account (sans
/// token) rides along on success so the UI can show who signed in without another round trip.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceLoginFinished {
    success: bool,
    account: Option<AccountView>,
    error: Option<String>,
}

/// Begin an OAuth device-flow sign-in. Returns immediately with the code to show the user, then
/// polls in the background and emits `device-login-finished` once they authorize (or it times out).
///
/// Not blocking: the poll can take a minute of the user typing a code into a browser, and holding
/// the command open that whole time would freeze the invoke. The start request itself is quick.
#[tauri::command]
pub fn device_login_start(
    app: AppHandle,
    provider: Provider,
    host: Option<String>,
) -> Result<DeviceLogin, String> {
    let (login, ctx) = auth::start_device_login(provider, host)?;
    std::thread::spawn(move || {
        let payload = match auth::poll_device_login(ctx) {
            Ok(account) => DeviceLoginFinished { success: true, account: Some(account), error: None },
            Err(e) => DeviceLoginFinished { success: false, account: None, error: Some(e) },
        };
        let _ = app.emit("device-login-finished", payload);
    });
    Ok(login)
}

/// Open a URL in the user's default browser — used by the sign-in dialog's "Open page" button to
/// land them on the device-verification page. Local action only.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    auth::open_browser(&url)
}

/// The GitHub/GitLab accounts signed in on this machine (never including their tokens).
#[tauri::command]
pub fn list_accounts() -> Vec<AccountView> {
    auth::accounts()
}

/// Sign out of one account, forgetting its stored token.
#[tauri::command]
pub fn sign_out(provider: Provider, host: String) -> Result<(), String> {
    auth::sign_out(provider, &host)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishRequest {
    /// The authored collection to publish.
    pub path: String,
    /// Host of the signed-in account to publish under (`github.com`, a GitLab host).
    pub host: String,
    /// Name for the new remote repository.
    pub repo_name: String,
    #[serde(default)]
    pub private: bool,
}

/// The outcome of publishing: the URL to share (students subscribe with it) and whether this call
/// created the remote or just pushed updates to an existing one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub url: String,
    pub created: bool,
}

/// Publish an authored collection: create the remote repository (first time) and push to it.
///
/// Idempotent for re-publishing: once the collection has an `origin`, later publishes just push the
/// new commits (the labs added since) rather than trying to create the repo again.
///
/// Blocking: creating a repo and pushing are network round trips, but small — the UI shows a pending
/// state, like an import.
#[tauri::command]
pub fn publish_collection(req: PublishRequest) -> Result<PublishResult, String> {
    let root = Path::new(&req.path);

    // Only the *first* publish needs an account (to create the repo). Re-publishing pushes to the
    // existing origin and authenticates via whatever token is stored for that remote's host, so it
    // needs no account argument — which is why the UI sends an empty host in that case.
    let (url, created) = match git::origin_url(root) {
        Some(existing) => (existing, false),
        None => {
            let account = auth::account_for_host(&req.host)
                .ok_or_else(|| format!("not signed in to {} — sign in first", req.host))?;
            let manifest = collection::load_manifest(root)?;
            let url = auth::create_remote_repo(
                &account,
                req.repo_name.trim(),
                &manifest.description,
                req.private,
            )?;
            git::set_remote(root, "origin", &url)?;
            (url, true)
        }
    };

    git::push(root)?;
    Ok(PublishResult { url, created })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishProjectRequest {
    /// The project to save to git.
    pub path: String,
    /// Host of the signed-in account to publish under, when a new repo has to be created.
    #[serde(default)]
    pub host: String,
    /// Name for the new remote repository; defaults to the project's name.
    #[serde(default)]
    pub repo_name: String,
    #[serde(default)]
    pub private: bool,
}

/// Save a project to the user's own git, or push updates to it.
///
/// If the project already has an `origin` the signed-in user owns, this just pushes the new commits —
/// the "update it" path for a lab they downloaded of their own. Otherwise (a fork with no remote, or
/// someone else's repo) it creates a fresh repository under the chosen account, points `origin` at it
/// and pushes — "save it to my git". Blocking: network round trips, like publishing a collection.
#[tauri::command]
pub fn publish_project(req: PublishProjectRequest) -> Result<PublishResult, String> {
    let root = Path::new(&req.path);
    let cfg = project::load(root).map_err(|e| format!("that folder is not a Koral project: {e}"))?;

    // Push straight to an origin the user owns; otherwise create a repo under their account first.
    let owned_origin = git::origin_url(root).filter(|u| auth::signed_in_owns(u));
    let (url, created) = match owned_origin {
        Some(existing) => (existing, false),
        None => {
            let host = req.host.trim();
            let account = auth::account_for_host(host)
                .ok_or_else(|| format!("not signed in to {host} — sign in first"))?;
            // A Hub-made project is already a repo; a hand-made one might not be. Ensure one exists.
            if git::info(root).is_none() {
                git::init(root)?;
            }
            let repo_name = req.repo_name.trim();
            let repo_name = if repo_name.is_empty() { cfg.name.trim() } else { repo_name };
            let url = auth::create_remote_repo(&account, repo_name, "", req.private)?;
            git::set_remote(root, "origin", &url)?;
            (url, true)
        }
    };

    git::push(root)?;
    Ok(PublishResult { url, created })
}

/// Overwrite a project's `koral.json` with settings edited in the Hub.
///
/// The build scaffolding bakes these values in (window flags in the CMake `run` target, content
/// dirs in the IDE launch configs), so it is regenerated on the next build — no need to do it
/// here, and doing it here would mean resolving the SDK just to change a window size.
#[tauri::command]
pub fn save_project_config(path: String, config: ProjectConfig) -> Result<(), String> {
    project::save(Path::new(&path), &config)
}

/// IDEs installed on this machine, for the per-project "Open in…" actions.
#[tauri::command]
pub fn installed_ides() -> Vec<ide::Ide> {
    ide::detect()
}

/// Open a project folder in an IDE.
///
/// The IDE's Run/Debug configuration is written during the build, so a project that has never
/// been built has no `run` target to launch yet. Generate it first, and the IDE is useful the
/// moment it opens rather than after the user has gone back to the Hub and pressed ▶.
/// An empty `ide_id` means "whatever the settings say" — so the card's Open button does not have
/// to know which IDE it is opening, and follows the preference without a round trip.
#[tauri::command]
pub fn open_in_ide(path: String, ide_id: Option<String>) -> Result<(), String> {
    let root = Path::new(&path);

    let ide_id = match ide_id.filter(|id| !id.is_empty()) {
        Some(id) => id,
        None => {
            settings::load()
                .default_ide()
                .ok_or("no IDE found on this machine — install VS Code or CLion")?
                .id
        }
    };

    let cfg = project::load(root)?;
    // resolve(), not ensure_installed() + read_manifest(): a source build carries no
    // framework.json of its own, and its manifest is derived from the tree each time.
    let (sdk_root, manifest) = framework::resolve(&cfg.framework_version)?;
    // The profile the Hub is set to, so the IDE opens on the same configuration ▶ builds — it is
    // the one whose run configuration and compile database the generated files preselect.
    scaffold::generate(root, &cfg, &sdk_root, &manifest, &project::profile(root))?;

    ide::open(&ide_id, root)
}

/// Default parent directory for new projects, for the create dialog.
#[tauri::command]
pub fn default_project_location() -> String {
    settings::load().project_location()
}

/// The Hub's preferences, as stored. Empty fields mean "no preference" — the *resolved* values
/// are reported by [`resolved_defaults`], so the settings panel can show what a blank field will
/// actually do without pretending the user chose it.
#[tauri::command]
pub fn settings() -> Settings {
    settings::load()
}

#[tauri::command]
pub fn save_settings(settings: Settings) -> Result<(), String> {
    settings::save(&settings)
}

/// What the empty settings currently resolve to, so the panel can render them as placeholders.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDefaults {
    pub project_location: String,
    pub ide_id: String,
    /// Empty when nothing is installed and the release list is unreachable — the panel says so
    /// rather than naming a version that would not resolve.
    pub framework_version: String,
}

#[tauri::command]
pub fn resolved_defaults() -> ResolvedDefaults {
    let s = settings::load();
    ResolvedDefaults {
        project_location: s.project_location(),
        ide_id: s.default_ide().map(|i| i.id).unwrap_or_default(),
        framework_version: s.framework_version().unwrap_or_default(),
    }
}

/// SDKs already installed on this machine.
#[tauri::command]
pub fn installed_frameworks() -> Vec<InstalledFramework> {
    framework::installed()
}

/// Releases publishing an SDK for this platform, newest first. Hits the network, so it can
/// fail (offline, rate-limited) — the UI shows the error rather than an empty list, because
/// "nothing to install" and "could not reach GitHub" mean very different things.
#[tauri::command]
pub fn available_frameworks() -> Result<Vec<AvailableFramework>, String> {
    framework::available()
}

/// Ensure a framework version is installed (downloading if needed) and return its SDK root.
#[tauri::command]
pub fn ensure_framework(version: String) -> Result<String, String> {
    framework::ensure_installed(&version).map(|p| p.to_string_lossy().into_owned())
}

/// Download progress for an SDK install, emitted as `framework-progress`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgress {
    version: String,
    downloaded: u64,
    /// 0 when the server sends no Content-Length — the UI must treat that as indeterminate.
    total: u64,
}

/// Emitted as `framework-finished` when an install ends, successfully or not.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallFinished {
    version: String,
    success: bool,
    error: Option<String>,
}

/// Download and unpack a framework release. Returns immediately; the download streams
/// `framework-progress` and ends with `framework-finished`.
///
/// Not a blocking command: these archives are ~40 MB, and a sync command would leave the UI
/// with no way to show a progress bar or tell the user anything is happening.
#[tauri::command]
pub fn install_framework(app: AppHandle, version: String) {
    std::thread::spawn(move || {
        let emitter = app.clone();
        let tag = version.clone();

        // Throttle to whole percent. Emitting per 64 KB chunk floods the webview's event
        // queue with ~640 events for a 40 MB download and makes the bar *less* responsive.
        let mut last_percent = u64::MAX;
        let result = framework::install(&version, |downloaded, total| {
            let percent = if total > 0 { downloaded * 100 / total } else { 0 };
            if percent != last_percent {
                last_percent = percent;
                let _ = emitter.emit(
                    "framework-progress",
                    InstallProgress { version: tag.clone(), downloaded, total },
                );
            }
        });

        let payload = match result {
            Ok(_) => InstallFinished { version, success: true, error: None },
            Err(e) => InstallFinished { version, success: false, error: Some(e) },
        };
        let _ = app.emit("framework-finished", payload);
    });
}

/// Delete an installed SDK. Idempotent, and leaves projects alone — a project referencing the
/// removed version simply re-downloads it on the next build (see `framework::ensure_installed`),
/// so this is safe to do to reclaim disk space.
#[tauri::command]
pub fn uninstall_framework(version: String) -> Result<(), String> {
    // A registered source build is the user's own build tree. Uninstall deletes a directory, so it
    // must never be pointed at one — forgetting it is the correct action, and the only one.
    if framework::is_source_pin(&version) || framework::local::find(&version).is_some() {
        return Err(
            "that is a framework built from source and registered by path. Remove the \
             registration instead — the Hub does not delete build trees it did not create."
                .to_string(),
        );
    }
    framework::uninstall(&version)
}

// --- Libraries (vcpkg) ------------------------------------------------------------------

/// Emitted as `vcpkg-progress` while the port tree is being cloned or updated, and once more at
/// the end with `done` or `error` set. One event stream for both, since the UI shows them the same
/// way — a line of text and a spinner.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VcpkgProgress {
    step: String,
    done: bool,
    error: Option<String>,
}

/// Report the Hub's vcpkg checkout to the UI as it changes.
fn emit_vcpkg(app: &AppHandle, progress: vcpkg::Progress) {
    let payload = match progress {
        vcpkg::Progress::Step(step) => {
            VcpkgProgress { step: step.to_string(), done: false, error: None }
        }
        vcpkg::Progress::Done { ports } => VcpkgProgress {
            step: format!("{ports} libraries available"),
            done: true,
            error: None,
        },
        vcpkg::Progress::Failed(e) => VcpkgProgress {
            step: String::new(),
            done: true,
            error: Some(e.to_string()),
        },
    };
    let _ = app.emit("vcpkg-progress", payload);
}

/// Clone the port tree in the background if this machine has none.
///
/// Called once at startup (see `lib.rs`) and again from the Libraries panel, so a first run that
/// was offline can be retried without restarting the Hub. Does nothing when a checkout is already
/// there or one is already being fetched.
pub fn ensure_vcpkg(app: AppHandle) {
    std::thread::spawn(move || vcpkg::ensure(|p| emit_vcpkg(&app, p)));
}

/// Whether libraries can be picked yet, and where they come from.
#[tauri::command]
pub fn vcpkg_status() -> vcpkg::Status {
    vcpkg::status()
}

/// Every port the checkout offers, alphabetically — the catalogue the library picker filters.
///
/// Purely local, and served from a cached index rather than by re-reading a few thousand port
/// manifests, so opening the picker costs one file read.
#[tauri::command]
pub fn vcpkg_ports() -> Vec<vcpkg::Port> {
    vcpkg::ports()
}

/// Fetch the latest ports. On demand only: a port tree that moved under a project between two
/// builds is exactly the surprise the Hub is trying to avoid.
///
/// Returns immediately; progress arrives as `vcpkg-progress`.
#[tauri::command]
pub fn update_vcpkg(app: AppHandle) {
    std::thread::spawn(move || vcpkg::update(|p| emit_vcpkg(&app, p)));
}

/// Run a build/run job on its own thread, addressed to `path`'s console.
///
/// Nothing here serializes the jobs: consoles are per project, so two projects can build at once
/// and each watches its own. The UI is what stops a project from being built twice over.
fn spawn_job<F>(app: AppHandle, path: String, job: F)
where
    F: FnOnce(&builder::Console) -> Result<(), String> + Send + 'static,
{
    // Off the command thread so the invoke resolves immediately; progress arrives as events.
    std::thread::spawn(move || {
        let console = builder::Console::new(&app, &path);
        console.finished(&job(&console));
    });
}

/// The profile a job runs in: the one asked for, or the one remembered for this project.
///
/// An unknown name is refused rather than passed through — it would become a CMake preset name
/// nothing defines, and the build would fail with a message about presets instead of about the
/// profile the caller actually got wrong.
fn resolve_profile(root: &Path, asked: Option<String>) -> Result<String, String> {
    match asked.filter(|p| !p.is_empty()) {
        Some(profile) if project::is_profile(&profile) => Ok(profile),
        Some(profile) => Err(format!(
            "'{profile}' is not a build profile — expected one of {}",
            project::PROFILES.join(", ")
        )),
        None => Ok(project::profile(root)),
    }
}

/// Configure + build a project. Streams `build-output`, then `build-finished`.
#[tauri::command]
pub fn build_project(app: AppHandle, path: String, profile: Option<String>) {
    let profile = match resolve_profile(Path::new(&path), profile) {
        Ok(profile) => profile,
        // No console has been opened for a job that never started, so report it the same way a
        // failed build is reported rather than dropping it.
        Err(e) => return spawn_job(app, path, move |_| Err(e)),
    };
    spawn_job(app, path, move |console| {
        builder::build_only(console, &profile)
    });
}

/// Build a project and launch it. Streams `build-output`, then `build-finished`.
#[tauri::command]
pub fn run_project(app: AppHandle, path: String, profile: Option<String>) {
    let profile = match resolve_profile(Path::new(&path), profile) {
        Ok(profile) => profile,
        Err(e) => return spawn_job(app, path, move |_| Err(e)),
    };
    spawn_job(app, path, move |console| builder::run(console, &profile));
}

/// Every build dependency and whether this machine has it — what the toolchain wizard shows.
///
/// Cheap and entirely local: a few `PATH` lookups and, on Windows, the cached MSVC probe. Safe to
/// call whenever the wizard opens or an install finishes.
#[tauri::command]
pub fn toolchain_status() -> Vec<toolchain::Tool> {
    toolchain::status()
}

/// Whether a build could run right now — everything present except vcpkg, which only a project
/// that declares libraries needs. Used to decide whether to open the wizard unprompted.
#[tauri::command]
pub fn toolchain_ready() -> bool {
    toolchain::ready()
}

/// Emitted as `tool-progress` while a build tool downloads.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolProgress {
    tool: toolchain::ToolId,
    downloaded: u64,
    total: u64,
}

/// Emitted as `tool-finished` when an install ends, either way.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolFinished {
    tool: toolchain::ToolId,
    success: bool,
    /// Where it landed, on success.
    path: Option<String>,
    error: Option<String>,
}

/// Download and unpack one of the tools the Hub can supply (CMake, Ninja).
///
/// Not blocking: CMake is ~50 MB, so this runs on its own thread and reports as `tool-progress`
/// then `tool-finished`, the same shape as a framework install.
#[tauri::command]
pub fn install_tool(app: AppHandle, tool: toolchain::ToolId) {
    std::thread::spawn(move || {
        let progress_app = app.clone();
        let result = toolchain::install(tool, |downloaded, total| {
            let _ = progress_app.emit(
                "tool-progress",
                ToolProgress { tool, downloaded, total },
            );
        });
        let payload = match result {
            Ok(path) => ToolFinished { tool, success: true, path: Some(path), error: None },
            Err(e) => ToolFinished { tool, success: false, path: None, error: Some(e) },
        };
        let _ = app.emit("tool-finished", payload);
    });
}

/// Start the platform's own compiler installer, and say what happened.
///
/// The Hub never installs a compiler itself — see `toolchain::install_compiler`. On Windows and
/// macOS this launches the official installer (which asks for its own approval and licence
/// acceptance); on Linux it returns the command to run, because a root password is not the Hub's to
/// ask for.
#[tauri::command]
pub fn install_compiler() -> Result<String, String> {
    toolchain::install_compiler()
}

/// The build profiles a project can be set to, and which one it is on.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profiles {
    pub available: Vec<String>,
    pub selected: String,
}

/// What ▶ and Build will use for this project, and what else it could be set to.
///
/// Machine-local: which configuration you are working in is not something a project carries to
/// someone else's machine, so it lives in the Hub's own state rather than in `koral.json` — the
/// same split that keeps `CMakePresets.json` out of git.
#[tauri::command]
pub fn project_profiles(path: String) -> Profiles {
    Profiles {
        available: project::PROFILES.iter().map(|p| p.to_string()).collect(),
        selected: project::profile(Path::new(&path)),
    }
}

/// Set the profile a project builds in. Takes effect on the next build — which regenerates the
/// IDE configuration too, so CLion and VS Code follow the Hub rather than drifting from it.
#[tauri::command]
pub fn set_project_profile(path: String, profile: String) -> Result<(), String> {
    project::set_profile(Path::new(&path), &profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Subscribing to your own published collection leaves the Hub holding two records of one
    /// collection — the local repo you author, and the URL you follow. Recognising that they are
    /// the same is what keeps it from being listed twice.
    #[test]
    fn a_subscription_to_your_own_collection_is_recognised() {
        let index = LocalIndex {
            projects: Vec::new(),
            authored: vec![
                ("/home/u/Koral/Collections/Labs".into(), "https://github.com/u/labs.git".into()),
                ("/home/u/Koral/Collections/Other".into(), "https://github.com/u/other.git".into()),
            ],
        };

        // The same collection, however the URL is spelled.
        for url in [
            "https://github.com/u/labs.git",
            "https://github.com/u/labs",
            "git@github.com:u/labs.git",
        ] {
            assert_eq!(
                index.authored_at(url).as_deref(),
                Some("/home/u/Koral/Collections/Labs"),
                "{url}"
            );
        }

        // Somebody else's collection is not yours, and must keep its own row.
        assert_eq!(index.authored_at("https://github.com/prof/labs.git"), None);
    }

    /// A collection entry can have more than one copy here — the project you added to your own
    /// collection, and a copy you later downloaded from it, which keeps its `origin` because you
    /// own the repository. Every copy has to be reported, or the sidebar hides one and the other
    /// shows a second time as an unrelated project.
    #[test]
    fn every_local_copy_of_an_entry_is_reported() {
        let index = LocalIndex {
            authored: Vec::new(),
            projects: vec![
                // The original, and a downloaded copy of the same repo in another spelling.
                (PathBuf::from("/home/u/Koral/Lab01"), Some("https://github.com/u/lab01.git".into())),
                (PathBuf::from("/home/u/Koral/lab01-1"), Some("git@github.com:u/lab01".into())),
                // Someone else's project, and one that was never published.
                (PathBuf::from("/home/u/Koral/Other"), Some("https://github.com/u/other.git".into())),
                (PathBuf::from("/home/u/Koral/Local"), None),
            ],
        };

        let found = index.local_paths_for("https://github.com/u/lab01", None);
        assert_eq!(found, vec!["/home/u/Koral/Lab01", "/home/u/Koral/lab01-1"]);

        // An entry nobody has a copy of is empty — that is what makes it show as downloadable.
        assert!(index.local_paths_for("https://github.com/u/nothing.git", None).is_empty());
    }

    /// This decides whether a collection entry shows as a project you already have or as something
    /// to download, so both directions matter: the spellings of one repository must match, and two
    /// different repositories must not.
    #[test]
    fn the_same_repository_matches_across_its_spellings() {
        let forms = [
            "https://github.com/course/lab01.git",
            "https://github.com/course/lab01",
            "https://github.com/course/lab01/",
            "git@github.com:course/lab01.git",
            "ssh://git@github.com/course/lab01",
            "https://GitHub.com/Course/Lab01.git",
        ];
        for a in forms {
            for b in forms {
                assert!(same_repo(a, b), "{a} should match {b}");
            }
        }

        for other in [
            "https://github.com/course/lab02.git",
            "https://github.com/student/lab01.git",
            "https://gitlab.com/course/lab01.git",
            "",
        ] {
            assert!(!same_repo(forms[0], other), "{other} should not match {}", forms[0]);
        }
    }
}
