//! Lab collections: browsable catalogs of downloadable starter projects ("labs").
//!
//! A collection is a small JSON manifest — [`CollectionManifest`] — published anywhere reachable
//! over HTTPS, listing labs by name and git URL. The Hub remembers the collection URLs a user has
//! added (per machine, exactly like the recent-projects index) and re-fetches each on open, so a
//! course can add or revise its labs after students have subscribed.
//!
//! A lab is an ordinary Koral project in its own git repository. Downloading someone else's lab
//! clones that repo, drops the upstream history and re-inits it as a fresh project, so the student
//! owns their copy and an upstream change can never clobber their edits. Downloading the signed-in
//! user's *own* repo instead keeps the history and `origin` (like `import_project`), so they can pull
//! and push updates rather than forking a disconnected copy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git;
use crate::model::{Kind, ProjectConfig};
use crate::paths;
use crate::project;

/// GitHub (and most hosts) require a User-Agent on API/raw requests and may 403 without one.
const USER_AGENT: &str = "KoralHub";

/// The manifest file the Hub looks for at the root of a collection repo when given a bare repo URL.
pub const MANIFEST_FILE: &str = "koral-collection.json";

/// Current collection-manifest schema version, stamped into manifests the Hub authors.
pub const SCHEMA_VERSION: u32 = 1;

// --- Manifest schema --------------------------------------------------------------------

/// A collection manifest: the document an instructor publishes to describe a course's labs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionManifest {
    /// Schema version, so the Hub can migrate older manifests forward. Optional: a hand-written
    /// manifest that omits it is treated as the current version rather than rejected.
    #[serde(default)]
    pub schema_version: u32,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// What the collection holds. Chosen at creation and enforced by [`add_lab`], so a course's
    /// lab list stays a lab list and a module registry stays a module registry. Manifests written
    /// before this existed default to projects — which is what they all were.
    #[serde(default)]
    pub contents: Contents,
    #[serde(default)]
    pub labs: Vec<Lab>,
}

/// What kind of entries a collection accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Contents {
    /// Runnable projects only — scenes and jobs. The default, and what every older manifest is.
    #[default]
    Projects,
    /// Modules only: a registry of reusable engine features.
    Modules,
    /// Anything goes — a course that ships its labs and the modules they use side by side.
    Mixed,
}

impl Contents {
    /// Whether an entry of this kind belongs in the collection. `None` — a repo whose kind cannot
    /// be read (no koral.json, or one from before `kind` existed) — is allowed everywhere, since
    /// refusing it would lock the very projects the feature predates out of their own collections.
    pub fn accepts(self, kind: Option<Kind>) -> bool {
        match (self, kind) {
            (_, None) => true,
            (Contents::Mixed, _) => true,
            (Contents::Projects, Some(k)) => k != Kind::Module,
            (Contents::Modules, Some(k)) => k == Kind::Module,
        }
    }

    /// Named in errors and the UI: what this collection says it holds.
    pub fn describe(self) -> &'static str {
        match self {
            Contents::Projects => "projects",
            Contents::Modules => "modules",
            Contents::Mixed => "projects and modules",
        }
    }
}

/// One entry in a collection: a downloadable lab, hosted in its own git repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lab {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// HTTPS git URL of the lab's own repository, cloned when the lab is downloaded.
    pub url: String,
    /// What the entry is (Scene/Job/Module), read from its own koral.json when it was added.
    /// Absent for entries added before this existed, or whose config could not be read — the UI
    /// shows those unbadged rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
}

// --- Subscribed-collections index (per machine, URLs only) ------------------------------

#[derive(Default, Serialize, Deserialize)]
struct CollectionCache {
    collections: Vec<CollectionEntry>,
}

#[derive(Serialize, Deserialize)]
struct CollectionEntry {
    url: String,
}

fn load_cache() -> CollectionCache {
    std::fs::read_to_string(paths::collections_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CollectionCache) -> Result<(), String> {
    let file = paths::collections_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Collection URLs the user has added, in the order added.
pub fn subscribed_urls() -> Vec<String> {
    load_cache().collections.into_iter().map(|e| e.url).collect()
}

/// Remember a collection URL. Idempotent — adding one already present is a no-op, so a student who
/// pastes the same link twice just refreshes it rather than getting a duplicate row.
pub fn add(url: &str) -> Result<(), String> {
    let url = url.trim().to_string();
    let mut cache = load_cache();
    if !cache.collections.iter().any(|c| c.url == url) {
        cache.collections.push(CollectionEntry { url });
    }
    save_cache(&cache)
}

/// Forget a collection URL (does not touch any labs already downloaded from it).
pub fn remove(url: &str) -> Result<(), String> {
    let mut cache = load_cache();
    cache.collections.retain(|c| c.url != url);
    // Drop the offline copy too: it is only ever read for a collection on the list above, so
    // leaving it would be dead weight that grows with every collection the user tries and drops.
    forget_manifest(url);
    save_cache(&cache)
}

// --- Fetching a manifest ----------------------------------------------------------------

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        // A manifest is a few kilobytes, and this runs on every open of the collections list — so
        // unlike an SDK download there is no legitimate slow case to leave room for. Without a
        // bound, a host that accepts the connection and then says nothing (a captive portal, a
        // hotel network) hangs the sidebar indefinitely instead of falling back to the cache.
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))
}

/// Pull `owner`/`repo` out of a GitHub repo URL, in either HTTPS or SSH form. `None` for anything
/// that is not a plain repo root (a `.../blob/...` or `.../tree/...` link has extra path segments).
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");

    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    // A third segment means this is a link *into* the repo (blob/tree/…), not the repo root.
    if parts.next().is_some() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Rewrite a `github.com/.../blob/<branch>/<path>` link to the matching `raw.githubusercontent.com`
/// URL, so a link a student copied straight from GitHub's file view actually serves JSON rather than
/// an HTML page. Anything that is not a GitHub blob link is returned unchanged.
fn github_blob_to_raw(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        if let Some((repo_path, file_path)) = rest.split_once("/blob/") {
            return format!("https://raw.githubusercontent.com/{repo_path}/{file_path}");
        }
    }
    url.to_string()
}

/// What a candidate URL is expected to serve, and so how to turn its bytes into a manifest.
#[derive(Clone, Copy)]
enum CandidateKind {
    /// A `koral-collection.json` — parsed directly as a [`CollectionManifest`].
    Json,
    /// A `.gitmodules` — each submodule becomes a lab, so a plain repo of submodules *is* a
    /// collection with no hand-written manifest to keep in sync.
    Gitmodules,
}

struct Candidate {
    url: String,
    kind: CandidateKind,
}

/// The locations to try, in order, to find a collection for what the user pasted.
///
/// Accepts several shapes so the user does not have to know the convention:
///  - a direct link to a `.json` manifest (used as-is, translating a GitHub blob link to raw),
///  - a GitHub repo URL — the manifest, then `.gitmodules`, on the default branch (`main`/`master`),
///  - anything else, treated as a base URL with the manifest/`.gitmodules` under it, then verbatim.
///
/// A `koral-collection.json` is always preferred over `.gitmodules`, since it carries lab
/// descriptions and a real title; `.gitmodules` is the zero-effort fallback.
fn manifest_candidates(input: &str) -> Vec<Candidate> {
    let url = input.trim().trim_end_matches('/');

    if url.ends_with(".json") {
        return vec![Candidate { url: github_blob_to_raw(url), kind: CandidateKind::Json }];
    }
    if let Some((owner, repo)) = github_owner_repo(url) {
        let raw = |file: &str, branch: &str| {
            format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{file}")
        };
        return vec![
            Candidate { url: raw(MANIFEST_FILE, "main"), kind: CandidateKind::Json },
            Candidate { url: raw(MANIFEST_FILE, "master"), kind: CandidateKind::Json },
            Candidate { url: raw(".gitmodules", "main"), kind: CandidateKind::Gitmodules },
            Candidate { url: raw(".gitmodules", "master"), kind: CandidateKind::Gitmodules },
        ];
    }
    vec![
        Candidate { url: format!("{url}/{MANIFEST_FILE}"), kind: CandidateKind::Json },
        Candidate { url: format!("{url}/.gitmodules"), kind: CandidateKind::Gitmodules },
        Candidate { url: url.to_string(), kind: CandidateKind::Json },
    ]
}

/// The parsed value returned after `key` (`path`, `url`) in a `.gitmodules` line, e.g. `url = X`.
fn config_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
    Some(rest.trim())
}

/// Derive labs from a `.gitmodules` file. Each `[submodule "…"]` section with a `url` becomes a
/// lab; its display name is the submodule `path` (or the section name), which is the folder an
/// instructor sees. Descriptions are unavailable in `.gitmodules`, so they come out empty.
fn labs_from_gitmodules(text: &str) -> Vec<Lab> {
    let mut labs = Vec::new();
    let mut section: Option<String> = None;
    let mut path: Option<String> = None;
    let mut url: Option<String> = None;

    // Emit the section accumulated so far, if it named a URL.
    let flush = |labs: &mut Vec<Lab>, section: &Option<String>, path: &mut Option<String>, url: &mut Option<String>| {
        if let Some(u) = url.take() {
            let name = path
                .take()
                .or_else(|| section.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| git::repo_name_from_url(&u));
            labs.push(Lab { name, description: String::new(), url: u, kind: None });
        } else {
            *path = None;
        }
    };

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut labs, &section, &mut path, &mut url);
            // `[submodule "the name"]` — the quoted part is the section name.
            section = line.split('"').nth(1).map(str::to_string);
        } else if let Some(v) = config_value(line, "path") {
            path = Some(v.to_string());
        } else if let Some(v) = config_value(line, "url") {
            url = Some(v.to_string());
        }
    }
    flush(&mut labs, &section, &mut path, &mut url);
    labs
}

/// Fetch and parse a collection for the given URL. Tries each candidate location and returns the
/// first that yields a usable collection; if none do, reports what the last attempt ran into so a
/// bad URL, an unreachable host and a malformed file read as the different problems they are.
pub fn fetch(url: &str) -> Result<CollectionManifest, String> {
    let client = http()?;
    let candidates = manifest_candidates(url);
    let mut last_err = String::from("no candidate URL to try");

    for candidate in &candidates {
        let text = match client
            .get(&candidate.url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
        {
            Ok(text) => text,
            Err(e) => {
                let unreachable = e.is_connect() || e.is_timeout();
                last_err = format!("{}: {e}", candidate.url);
                // The candidates are alternative paths into the *same* repository, not
                // alternative hosts — so a host that cannot be reached at all will not become
                // reachable for the next one. Offline, trying the rest only multiplies the
                // timeout the collections list waits through before falling back to the cache.
                // A 404 is different: that is a real answer, and the next candidate may exist.
                if unreachable {
                    break;
                }
                continue;
            }
        };

        match candidate.kind {
            CandidateKind::Json => match serde_json::from_str::<CollectionManifest>(&text) {
                Ok(manifest) => return Ok(manifest),
                Err(e) => last_err = format!("{}: not a valid collection manifest ({e})", candidate.url),
            },
            CandidateKind::Gitmodules => {
                let labs = labs_from_gitmodules(&text);
                if labs.is_empty() {
                    last_err = format!("{}: no submodules to offer as labs", candidate.url);
                } else {
                    return Ok(CollectionManifest {
                        schema_version: SCHEMA_VERSION,
                        title: git::repo_name_from_url(url),
                        description: String::new(),
                        contents: Contents::default(),
                        labs,
                    });
                }
            }
        }
    }
    Err(format!("could not load a collection from {url} — {last_err}"))
}

// --- Offline cache of fetched manifests -------------------------------------------------

/// Last-known-good manifest per collection URL. See [`paths::collection_manifests_file`].
#[derive(Default, Serialize, Deserialize)]
struct ManifestCache {
    #[serde(default)]
    manifests: std::collections::BTreeMap<String, CollectionManifest>,
}

fn load_manifest_cache() -> ManifestCache {
    std::fs::read_to_string(paths::collection_manifests_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_manifest_cache(cache: &ManifestCache) -> Result<(), String> {
    let file = paths::collection_manifests_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Remember what this collection last looked like, so it survives going offline.
fn cache_manifest(url: &str, manifest: &CollectionManifest) {
    let mut cache = load_manifest_cache();
    cache.manifests.insert(url.trim().to_string(), manifest.clone());
    // Best-effort: a cache that cannot be written costs the *next* offline open, not this one.
    if let Err(e) = save_manifest_cache(&cache) {
        eprintln!("koral-hub: could not cache the manifest for {url}: {e}");
    }
}

/// Forget a collection’s cached manifest. Called when it is unsubscribed from, so the cache does
/// not accumulate collections the user has left.
fn forget_manifest(url: &str) {
    let mut cache = load_manifest_cache();
    if cache.manifests.remove(url.trim()).is_some() {
        let _ = save_manifest_cache(&cache);
    }
}

/// How a collection’s contents were obtained — freshly fetched, or served from the cache because
/// the fetch failed. The UI needs the difference: stale contents are still worth showing and
/// working from, but must not be presented as current.
pub struct Fetched {
    pub manifest: CollectionManifest,
    /// True when this came from the cache after a failed fetch, with the reason it failed.
    pub stale: Option<String>,
}

/// Fetch a collection, falling back to the last copy that worked when the network does not.
///
/// This is what makes a subscribed collection usable offline. [`fetch`] is still the honest
/// network answer and is what validates a URL on subscribe — nothing should be added to the list
/// on the strength of a cache. But *listing* collections happens on every open, including the
/// ones with no network, and there a collection that is merely unreachable should not read as a
/// collection that is empty or broken.
///
/// A successful fetch always refreshes the cache, so a course revising its labs still propagates
/// the moment the student is online.
pub fn fetch_or_cached(url: &str) -> Result<Fetched, String> {
    match fetch(url) {
        Ok(manifest) => {
            cache_manifest(url, &manifest);
            Ok(Fetched { manifest, stale: None })
        }
        // Nothing cached either: this really is a collection the Hub cannot show, so say so.
        Err(e) => match load_manifest_cache().manifests.remove(url.trim()) {
            Some(manifest) => Ok(Fetched { manifest, stale: Some(e) }),
            None => Err(e),
        },
    }
}

// --- Downloading a lab ------------------------------------------------------------------

/// Download a lab into `location/<repo-name>`, returning its root and config.
///
/// Clones the lab's repo, refuses if the target folder already exists (never clobbers local work),
/// and deletes the clone again if it turns out not to be a Koral project. On success, a repo the
/// signed-in user owns keeps its history and `origin` (so they can update it); anyone else's has its
/// upstream history stripped and re-inited, so the download behaves like a brand-new project.
pub fn download_lab(url: &str, location: &Path) -> Result<(PathBuf, ProjectConfig), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("this lab has no download URL".into());
    }
    let name = git::repo_name_from_url(url);
    if name.is_empty() {
        return Err("could not work out a folder name from the lab's URL".into());
    }

    let root = location.join(&name);
    if root.exists() {
        return Err(format!(
            "a folder named '{name}' already exists in {}",
            location.display()
        ));
    }

    git::clone(url, &root)?;

    let cfg = match project::load(&root) {
        Ok(cfg) => cfg,
        Err(e) => {
            // Not a Koral project — don't leave the clone lying around.
            let _ = std::fs::remove_dir_all(&root);
            return Err(format!(
                "downloaded, but it has no valid {} — it does not look like a Koral lab ({e})",
                project::CONFIG_FILE
            ));
        }
    };

    if crate::auth::signed_in_owns(url) {
        // The signed-in user's own repository: keep its history and `origin` so they can pull and
        // push updates, exactly like an import. No fork — updating it updates the published lab.
    } else {
        // Someone else's repository: drop the upstream history and remote, then re-init, so the
        // student's edits are their own and an upstream pull can never overwrite them. They can later
        // save the copy to their own git (publish). Best-effort, like `project::create`.
        let _ = std::fs::remove_dir_all(root.join(".git"));
        if let Err(e) = git::init(&root) {
            eprintln!("koral-hub: git init failed for downloaded lab {}: {e}", root.display());
        }
    }

    project::add_recent(&root)?;
    // Record where it came from. For a repository the user owns this merely duplicates the
    // `origin` kept above, but for anyone else's it is the only surviving link back to the
    // collection entry: the branch above threw the remote away with the history. Without it the
    // download lists as a loose project instead of under the collection it came from.
    if let Err(e) = project::set_source_url(&root, url) {
        eprintln!("koral-hub: could not record the source of {}: {e}", root.display());
    }
    Ok((root, cfg))
}

/// Where a subscribed collection's downloads live: `<location>/<the collection's repo name>/`.
///
/// Downloads used to land straight in the general projects folder, which left a course's labs
/// scattered among everything else the user had. Giving each collection a folder of its own keeps
/// them together on disk the way they are grouped in the sidebar.
///
/// Named from the URL rather than from the manifest title, for two reasons. The title is written
/// by whoever publishes the collection, and a remote-supplied string has no business becoming a
/// path component unchecked. And a course can revise its title whenever it likes, which would
/// strand every lab already downloaded under the old name. The URL is the one stable thing a
/// subscription has.
///
/// An empty URL means "not from a collection" and yields `location` unchanged — the flat layout,
/// which is still right for an authored collection, whose entries already live inside its repo.
pub fn downloads_dir(location: &Path, collection_url: &str) -> Result<PathBuf, String> {
    let url = collection_url.trim();
    if url.is_empty() {
        return Ok(location.to_path_buf());
    }

    let name = git::repo_name_from_url(url);
    // `repo_name_from_url` returns the last path segment, so it can never contain a separator —
    // but "." and ".." would still resolve somewhere other than a child of `location`, and this
    // string comes from a URL the user pasted. Refuse them rather than build the path.
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("could not work out a folder name for the collection at {url}"));
    }

    let dir = location.join(name);
    // Created here rather than left to the clone: `git::clone` makes the repo directory, not the
    // collection directory above it, and would fail on the missing parent the very first time.
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    Ok(dir)
}

// --- Authoring a collection -------------------------------------------------------------

/// Read a local collection's own `koral-collection.json`.
pub fn load_manifest(root: &Path) -> Result<CollectionManifest, String> {
    let file = root.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", file.display()))
}

fn write_manifest(root: &Path, manifest: &CollectionManifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?;
    std::fs::write(root.join(MANIFEST_FILE), text)
        .map_err(|e| format!("failed to write {MANIFEST_FILE}: {e}"))
}

/// Render the human-facing README for a collection — its title, description, and a list of labs.
/// Regenerated on every edit so it never drifts from the manifest.
fn write_readme(root: &Path, manifest: &CollectionManifest) -> Result<(), String> {
    let mut md = format!("# {}\n\n", manifest.title);
    if !manifest.description.trim().is_empty() {
        md.push_str(manifest.description.trim());
        md.push_str("\n\n");
    }
    md.push_str(&format!(
        "A [Koral](https://github.com/radueduard/Koral) collection of {}. In Koral Hub, open \
         **Collections \u{2192} Browse**, add this repository's URL, and download any entry.\n\n",
        manifest.contents.describe(),
    ));
    // The heading follows the contents so a module registry doesn't call its modules "labs".
    md.push_str(match manifest.contents {
        Contents::Projects => "## Labs\n\n",
        Contents::Modules => "## Modules\n\n",
        Contents::Mixed => "## Contents\n\n",
    });
    if manifest.labs.is_empty() {
        md.push_str("_Nothing here yet._\n");
    } else {
        for lab in &manifest.labs {
            // In a mixed collection the marker is what tells the two kinds apart at a glance;
            // in a modules-only one every row would say it, so it says nothing.
            let marker = match (manifest.contents, lab.kind) {
                (Contents::Mixed, Some(Kind::Module)) => " *(module)*",
                _ => "",
            };
            md.push_str(&format!("- **{}**{marker} — <{}>", lab.name, lab.url));
            if !lab.description.trim().is_empty() {
                md.push_str(&format!("  \n  {}", lab.description.trim()));
            }
            md.push('\n');
        }
    }
    std::fs::write(root.join("README.md"), md).map_err(|e| format!("failed to write README.md: {e}"))
}

/// Scaffold a new, empty collection repo under `location/name` and return its root.
///
/// It gets a README, a seeded `koral-collection.json` and an initial git commit — labs are added
/// afterwards as submodules (see [`add_lab`]). Fails if the folder already exists, so it never
/// clobbers local work. Unlike a project, git is *required*: a collection aggregates other repos as
/// submodules, so it must be a repo itself.
pub fn create(
    location: &Path,
    name: &str,
    description: &str,
    contents: Contents,
) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("collection name cannot be empty".into());
    }
    let root = location.join(name);
    if root.exists() {
        return Err(format!(
            "a folder named '{name}' already exists in {}",
            location.display()
        ));
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let manifest = CollectionManifest {
        schema_version: SCHEMA_VERSION,
        title: name.to_string(),
        description: description.trim().to_string(),
        contents,
        labs: Vec::new(),
    };
    write_manifest(&root, &manifest)?;
    write_readme(&root, &manifest)?;
    git::init(&root)?;

    Ok(root)
}

/// Add a lab to a local collection as a git submodule, then record it in the manifest and README
/// and commit. `name`/`description` are optional display metadata; the folder (and submodule path)
/// always comes from the lab's repo name.
///
/// The submodule clone happens *before* anything is written, so a failure — a bad URL, a private
/// repo, no network — leaves the collection exactly as it was rather than half-edited.
pub fn add_lab(
    root: &Path,
    url: &str,
    name: Option<&str>,
    description: &str,
) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("enter the lab's git URL".into());
    }
    let sub_path = git::repo_name_from_url(url);
    if sub_path.is_empty() {
        return Err("could not work out a folder name from the lab's URL".into());
    }

    let mut manifest = load_manifest(root)?;
    if manifest.labs.iter().any(|l| l.url == url) {
        return Err(format!("this collection already includes {url}"));
    }

    git::submodule_add(root, url, &sub_path)?;

    // Now that the submodule's working tree exists, its own koral.json says what it is — which is
    // both the badge the manifest records and the thing the collection's contents rule checks.
    // Unreadable (not a Koral repo, or a config from before `kind` existed) stays None, which every
    // contents rule accepts; see Contents::accepts.
    let kind = project::load(&root.join(&sub_path)).ok().map(|cfg| cfg.kind);
    if !manifest.contents.accepts(kind) {
        // The clone happened, so undo it — a rejected add must leave the collection untouched,
        // exactly as a failed clone does.
        let _ = git::submodule_remove(root, &sub_path);
        let what = match kind {
            Some(Kind::Module) => "a module",
            _ => "a project",
        };
        return Err(format!(
            "'{sub_path}' is {what}, but this collection holds {} only",
            manifest.contents.describe()
        ));
    }

    let display = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| sub_path.clone());
    manifest.labs.push(Lab {
        name: display,
        description: description.trim().to_string(),
        url: url.to_string(),
        kind,
    });
    write_manifest(root, &manifest)?;
    write_readme(root, &manifest)?;
    git::commit_all(root, &format!("Add lab {sub_path}"))
}

/// Remove a lab from a local collection: drop its submodule, its manifest entry and its README line,
/// then commit. Identified by URL, which is what the manifest stores. Errors if the collection does
/// not include it.
pub fn remove_lab(root: &Path, url: &str) -> Result<(), String> {
    let mut manifest = load_manifest(root)?;
    let idx = manifest
        .labs
        .iter()
        .position(|l| l.url == url)
        .ok_or("that project is not in this collection")?;
    let lab = manifest.labs.remove(idx);

    // The submodule path is derived from the URL, exactly as `add_lab` derived it.
    let sub_path = git::repo_name_from_url(&lab.url);
    git::submodule_remove(root, &sub_path)?;

    write_manifest(root, &manifest)?;
    write_readme(root, &manifest)?;
    git::commit_all(root, &format!("Remove lab {sub_path}"))
}

/// Move a lab one place earlier (`up`) or later within the collection, then commit. Reordering is
/// purely presentational — the display list, manifest and README order — so it never touches the
/// submodules themselves. A no-op (still `Ok`) when the lab is already at that end.
pub fn reorder_lab(root: &Path, url: &str, up: bool) -> Result<(), String> {
    let mut manifest = load_manifest(root)?;
    let i = manifest
        .labs
        .iter()
        .position(|l| l.url == url)
        .ok_or("that project is not in this collection")?;
    let j = if up {
        i.checked_sub(1)
    } else {
        (i + 1 < manifest.labs.len()).then_some(i + 1)
    };
    let Some(j) = j else {
        return Ok(()); // already at the top / bottom
    };
    manifest.labs.swap(i, j);

    write_manifest(root, &manifest)?;
    write_readme(root, &manifest)?;
    git::commit_all(root, "Reorder labs")
}

// --- Authored-collections index (per machine, paths only) -------------------------------

fn load_authored() -> CollectionCache {
    std::fs::read_to_string(paths::authored_collections_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_authored(cache: &CollectionCache) -> Result<(), String> {
    let file = paths::authored_collections_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    std::fs::write(file, text).map_err(|e| e.to_string())
}

/// Local collection repos the user is authoring, most-recent first, that still exist on disk.
pub fn authored_paths() -> Vec<PathBuf> {
    load_authored()
        .collections
        .into_iter()
        .map(|e| PathBuf::from(e.url))
        .filter(|p| p.exists())
        .collect()
}

/// Add (or move to front) a locally-authored collection in the index. Called by the command layer
/// after [`create`] succeeds, mirroring how `project::add_recent` follows `project::create`.
pub fn add_authored(root: &Path) -> Result<(), String> {
    let key = root.to_string_lossy().into_owned();
    let mut cache = load_authored();
    cache.collections.retain(|e| e.url != key);
    cache.collections.insert(0, CollectionEntry { url: key });
    save_authored(&cache)
}

/// Drop an authored collection from the index, optionally deleting its folder.
///
/// The delete guard mirrors `project::delete`: refuse to recursively remove a directory that has no
/// `koral-collection.json`, so a path that round-tripped through the UI can never erase something
/// that is not a collection.
pub fn delete_authored(root: &Path, delete_files: bool) -> Result<(), String> {
    if delete_files {
        if !root.join(MANIFEST_FILE).is_file() {
            return Err(format!(
                "refusing to delete {}: it has no {MANIFEST_FILE}, so it is not a collection",
                root.display()
            ));
        }
        std::fs::remove_dir_all(root)
            .map_err(|e| format!("failed to delete {}: {e}", root.display()))?;
    }
    let key = root.to_string_lossy().into_owned();
    let mut cache = load_authored();
    cache.collections.retain(|e| e.url != key);
    save_authored(&cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the offline cache: a collection whose host cannot be reached still
    /// reports its entries, flagged as stale rather than failing. Without this a student with no
    /// network opens the Hub to an empty, “unavailable” collection and loses the grouping of the
    /// labs they already downloaded.
    ///
    /// Port 9 (discard) on the loopback refuses immediately, so this is an offline machine’s
    /// behaviour without the wait or a real network.
    #[test]
    fn an_unreachable_collection_falls_back_to_its_cached_contents() {
        let url = "https://127.0.0.1:9/koral-test-offline-fallback.json";
        let manifest = CollectionManifest {
            schema_version: SCHEMA_VERSION,
            title: "Graphics Labs".into(),
            description: String::new(),
            contents: Contents::Projects,
            labs: vec![Lab {
                name: "Lab 01".into(),
                description: String::new(),
                url: "https://github.com/prof/lab01".into(),
                kind: None,
            }],
        };

        // Nothing cached yet: an unreachable collection with no history is a real failure.
        forget_manifest(url);
        assert!(fetch_or_cached(url).is_err(), "no cache means no contents to show");

        cache_manifest(url, &manifest);
        let got = fetch_or_cached(url).expect("the cached copy should stand in");
        assert_eq!(got.manifest.title, "Graphics Labs");
        assert_eq!(got.manifest.labs.len(), 1);
        assert!(got.stale.is_some(), "served from cache, so it must be marked stale");

        // The fallback must not consume the cache — every later open is offline too.
        assert!(fetch_or_cached(url).is_ok(), "the cache survives being read");

        forget_manifest(url);
        assert!(fetch_or_cached(url).is_err(), "unsubscribing drops the cached copy");
    }

    /// The point of the collection folder: a course’s labs land together under a folder named for
    /// the collection, not loose beside every other project the user has.
    #[test]
    fn downloads_land_in_a_folder_named_for_the_collection() {
        let projects = std::env::temp_dir().join("koral-downloads-dir-test");
        let _ = std::fs::remove_dir_all(&projects);

        let dir = downloads_dir(&projects, "https://github.com/prof/graphics-labs.git").unwrap();
        assert_eq!(dir, projects.join("graphics-labs"));
        // The clone needs the folder to exist, so it is created rather than merely computed.
        assert!(dir.is_dir());

        // The same collection spelled differently is still the same folder — labs must not end up
        // split across two of them because a URL gained a suffix or a trailing slash.
        for spelling in [
            "https://github.com/prof/graphics-labs",
            "https://github.com/prof/graphics-labs/",
            "git@github.com:prof/graphics-labs.git",
        ] {
            assert_eq!(downloads_dir(&projects, spelling).unwrap(), dir, "{spelling}");
        }

        let _ = std::fs::remove_dir_all(&projects);
    }

    /// No collection means the old flat layout, which is what an authored collection still wants.
    #[test]
    fn no_collection_url_leaves_the_location_alone() {
        let projects = std::env::temp_dir().join("koral-downloads-dir-none");
        assert_eq!(downloads_dir(&projects, "").unwrap(), projects);
        assert_eq!(downloads_dir(&projects, "   ").unwrap(), projects);
        // Nothing was created for a path that is only being passed through.
        assert!(!projects.exists());
    }

    /// A pasted URL must never name a folder outside the projects directory.
    #[test]
    fn a_url_that_names_no_usable_folder_is_refused() {
        let projects = std::env::temp_dir().join("koral-downloads-dir-bad");
        for bad in ["https://example.com/labs/..", "https://example.com/labs/."] {
            assert!(downloads_dir(&projects, bad).is_err(), "{bad}");
        }
        assert!(!projects.exists());
    }

    fn candidate_urls(input: &str) -> Vec<String> {
        manifest_candidates(input).into_iter().map(|c| c.url).collect()
    }

    #[test]
    fn github_repo_urls_try_the_manifest_then_gitmodules_on_the_default_branch() {
        let got = candidate_urls("https://github.com/prof/graphics-labs");
        assert_eq!(
            got,
            vec![
                format!("https://raw.githubusercontent.com/prof/graphics-labs/main/{MANIFEST_FILE}"),
                format!("https://raw.githubusercontent.com/prof/graphics-labs/master/{MANIFEST_FILE}"),
                "https://raw.githubusercontent.com/prof/graphics-labs/main/.gitmodules".to_string(),
                "https://raw.githubusercontent.com/prof/graphics-labs/master/.gitmodules".to_string(),
            ]
        );
        // A ".git" suffix and trailing slash must not change the result.
        assert_eq!(candidate_urls("https://github.com/prof/graphics-labs.git/"), got);
    }

    #[test]
    fn a_direct_json_link_is_used_as_is_but_a_blob_link_becomes_raw() {
        assert_eq!(
            candidate_urls("https://example.com/course/labs.json"),
            vec!["https://example.com/course/labs.json".to_string()]
        );
        assert_eq!(
            candidate_urls("https://github.com/prof/labs/blob/main/koral-collection.json"),
            vec!["https://raw.githubusercontent.com/prof/labs/main/koral-collection.json".to_string()]
        );
    }

    #[test]
    fn a_link_into_a_repo_is_not_mistaken_for_the_repo_root() {
        // ".../tree/main/sub" has extra segments, so it is not treated as a repo whose default
        // branch we can guess — it falls through to the base-URL candidates instead.
        assert_eq!(
            candidate_urls("https://github.com/prof/labs/tree/main/sub"),
            vec![
                format!("https://github.com/prof/labs/tree/main/sub/{MANIFEST_FILE}"),
                "https://github.com/prof/labs/tree/main/sub/.gitmodules".to_string(),
                "https://github.com/prof/labs/tree/main/sub".to_string(),
            ]
        );
        assert!(github_owner_repo("https://github.com/prof/labs/tree/main/sub").is_none());
    }

    #[test]
    fn gitmodules_becomes_a_lab_per_submodule() {
        let text = r#"
[submodule "lab01-triangle"]
	path = lab01-triangle
	url = https://github.com/course/lab01-triangle.git
[submodule "lab02"]
	path = labs/texturing
	url = https://github.com/course/lab02-texturing
"#;
        let labs = labs_from_gitmodules(text);
        assert_eq!(labs.len(), 2);
        // The display name is the submodule `path`, and the URL is carried through verbatim.
        assert_eq!(labs[0].name, "lab01-triangle");
        assert_eq!(labs[0].url, "https://github.com/course/lab01-triangle.git");
        assert_eq!(labs[1].name, "labs/texturing");
        assert_eq!(labs[1].description, "");
    }

    #[test]
    fn manifest_parses_with_optional_fields_omitted() {
        let m: CollectionManifest = serde_json::from_str(
            r#"{ "title": "Intro to Graphics",
                 "labs": [ { "name": "Lab 01", "url": "https://github.com/c/lab01.git" } ] }"#,
        )
        .expect("a minimal manifest should parse");
        assert_eq!(m.title, "Intro to Graphics");
        assert_eq!(m.description, "");
        assert_eq!(m.labs.len(), 1);
        assert_eq!(m.labs[0].description, "");
    }

    /// End-to-end of the authoring flow with a *local* repo standing in for the lab, so the real
    /// libgit2 submodule machinery (add, clone, finalize, commit) is exercised without a network.
    #[test]
    fn add_lab_adds_a_submodule_and_records_it() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-authoring-test-{n}"));
        std::fs::create_dir_all(&base).unwrap();

        // A local "lab" repo to be added as a submodule (a committed folder is enough).
        let lab = base.join("lab01-triangle");
        std::fs::create_dir_all(&lab).unwrap();
        std::fs::write(lab.join("koral.json"), "{}").unwrap();
        git::init(&lab).unwrap();

        let collection = create(&base, "Graphics Labs", "", Contents::default()).unwrap();
        let lab_url = lab.to_string_lossy().into_owned();
        add_lab(&collection, &lab_url, Some("Lab 01 — Triangle"), "First triangle").unwrap();

        // The submodule is checked out, and .gitmodules records it.
        assert!(collection.join("lab01-triangle/koral.json").is_file());
        let gitmodules = std::fs::read_to_string(collection.join(".gitmodules")).unwrap();
        assert!(gitmodules.contains("lab01-triangle"), "{gitmodules}");

        // The manifest carries the display metadata; the README lists the lab.
        let manifest = load_manifest(&collection).unwrap();
        assert_eq!(manifest.labs.len(), 1);
        assert_eq!(manifest.labs[0].name, "Lab 01 — Triangle");
        assert_eq!(manifest.labs[0].description, "First triangle");
        assert_eq!(manifest.labs[0].url, lab_url);
        assert!(std::fs::read_to_string(collection.join("README.md")).unwrap().contains("Lab 01"));

        // Adding the same lab twice is refused rather than producing a duplicate submodule.
        assert!(add_lab(&collection, &lab_url, None, "").is_err());

        std::fs::remove_dir_all(&base).ok();
    }

    /// Reordering swaps manifest order; removing drops the submodule, its working tree and its
    /// `.gitmodules` entry — exercising the real libgit2 teardown, not just the manifest.
    #[test]
    fn reorder_then_remove_labs() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-remove-test-{n}"));
        std::fs::create_dir_all(&base).unwrap();

        // Two local "lab" repos to add as submodules.
        let mut urls = Vec::new();
        for name in ["lab01", "lab02"] {
            let lab = base.join(name);
            std::fs::create_dir_all(&lab).unwrap();
            std::fs::write(lab.join("koral.json"), "{}").unwrap();
            git::init(&lab).unwrap();
            urls.push(lab.to_string_lossy().into_owned());
        }

        let collection = create(&base, "Labs", "", Contents::default()).unwrap();
        add_lab(&collection, &urls[0], None, "").unwrap();
        add_lab(&collection, &urls[1], None, "").unwrap();

        // Reorder: lab02 moves ahead of lab01.
        reorder_lab(&collection, &urls[1], true).unwrap();
        let manifest = load_manifest(&collection).unwrap();
        assert_eq!(manifest.labs[0].url, urls[1]);
        assert_eq!(manifest.labs[1].url, urls[0]);

        // Remove lab01: gone from the manifest, the working tree and .gitmodules; lab02 remains.
        remove_lab(&collection, &urls[0]).unwrap();
        let manifest = load_manifest(&collection).unwrap();
        assert_eq!(manifest.labs.len(), 1);
        assert_eq!(manifest.labs[0].url, urls[1]);
        assert!(!collection.join("lab01").exists(), "working tree should be gone");
        assert!(collection.join("lab02/koral.json").is_file(), "the other lab stays");
        let gitmodules = std::fs::read_to_string(collection.join(".gitmodules")).unwrap();
        assert!(!gitmodules.contains("lab01"), "{gitmodules}");
        assert!(gitmodules.contains("lab02"), "{gitmodules}");

        std::fs::remove_dir_all(&base).ok();
    }

    /// The contents rule, in every direction that matters: what each collection type accepts, and
    /// that an unknown kind (a pre-`kind` repo) is accepted everywhere rather than locked out.
    #[test]
    fn contents_accepts_the_right_kinds() {
        use Contents::*;
        assert!(Projects.accepts(Some(Kind::Scene)));
        assert!(Projects.accepts(Some(Kind::Job)));
        assert!(!Projects.accepts(Some(Kind::Module)));
        assert!(!Modules.accepts(Some(Kind::Scene)));
        assert!(Modules.accepts(Some(Kind::Module)));
        assert!(Mixed.accepts(Some(Kind::Scene)));
        assert!(Mixed.accepts(Some(Kind::Module)));
        for contents in [Projects, Modules, Mixed] {
            assert!(contents.accepts(None), "unknown kinds must be allowed in {contents:?}");
        }
    }

    /// A manifest without `contents` — every manifest written before it existed — reads as a
    /// projects collection, which is what they all were.
    #[test]
    fn manifests_without_contents_default_to_projects() {
        let m: CollectionManifest =
            serde_json::from_str(r#"{ "title": "Old", "labs": [] }"#).unwrap();
        assert_eq!(m.contents, Contents::Projects);
    }

    /// End-to-end of the enforcement: a modules-only collection takes a module (stamping its kind
    /// in the manifest) and refuses a scene — leaving no half-added submodule behind.
    #[test]
    fn a_modules_collection_takes_modules_and_refuses_projects() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-contents-test-{n}"));
        std::fs::create_dir_all(&base).unwrap();

        // Two local repos with real configs: one module, one scene.
        let write_repo = |name: &str, kind: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("koral.json"),
                format!(
                    r#"{{ "schemaVersion":1, "name":"{name}", "color":[1,0,0],
                         "frameworkVersion":"0.0.9", "kind":"{kind}" }}"#
                ),
            )
            .unwrap();
            git::init(&dir).unwrap();
            dir.to_string_lossy().into_owned()
        };
        let module_url = write_repo("cameras", "Module");
        let scene_url = write_repo("game", "Scene");

        let registry = create(&base, "Modules", "", Contents::Modules).unwrap();

        add_lab(&registry, &module_url, None, "").unwrap();
        let manifest = load_manifest(&registry).unwrap();
        assert_eq!(manifest.labs.len(), 1);
        assert_eq!(manifest.labs[0].kind, Some(Kind::Module), "the kind is stamped on add");

        let err = add_lab(&registry, &scene_url, None, "").unwrap_err();
        assert!(err.contains("modules"), "{err}");
        // The rejected add rolled its submodule back: nothing on disk, nothing in the manifest.
        assert!(!registry.join("game").exists(), "rejected submodule must be rolled back");
        assert_eq!(load_manifest(&registry).unwrap().labs.len(), 1);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn create_scaffolds_a_committed_collection_repo_with_a_manifest() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("koral-collection-test-{n}"));
        std::fs::create_dir_all(&base).unwrap();

        let root = create(&base, "Graphics Labs", "Labs for CS-4560.", Contents::default()).unwrap();
        assert!(root.join(".git").is_dir(), "a collection must be a git repo");
        assert!(root.join("README.md").is_file());

        let manifest = load_manifest(&root).unwrap();
        assert_eq!(manifest.title, "Graphics Labs");
        assert_eq!(manifest.description, "Labs for CS-4560.");
        assert!(manifest.labs.is_empty(), "a fresh collection has no labs");
        assert_eq!(manifest.schema_version, SCHEMA_VERSION);

        std::fs::remove_dir_all(&base).ok();
    }
}
