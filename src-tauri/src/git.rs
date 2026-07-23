//! Native git via libgit2 (vendored, statically linked) — no system `git` binary required.
//!
//! Three jobs, all HTTPS-only:
//!  - [`init`]  — turn a freshly scaffolded project into a repo with one initial commit,
//!  - [`clone`] — pull a project down from a remote when importing,
//!  - [`info`]  — read the little bit of status the project cards show (branch, dirty, remote).

use std::path::Path;

use git2::{
    Cred, CredentialType, Error as GitError, IndexAddOption, PushOptions, RemoteCallbacks,
    Repository, Signature, StatusOptions,
};
use serde::Serialize;

/// A small git summary for a project card. The command layer sends `None` (not a repo) or this.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    /// Current branch short name, or `None` on a detached / unborn HEAD.
    pub branch: Option<String>,
    /// Uncommitted changes in the working tree or index (untracked files included).
    pub dirty: bool,
    /// `origin` URL when the project has one — i.e. it was cloned or wired to an upstream.
    pub remote: Option<String>,
}

/// Initialise a repo at `root` and commit everything currently there (honouring `.gitignore`).
///
/// Best-effort by contract: a project is perfectly usable without git, so `project::create` treats
/// a failure here as non-fatal. Uses the machine's configured git identity when there is one, and
/// falls back to a Hub identity so the first commit never fails on a box that has never run
/// `git config`.
pub fn init(root: &Path) -> Result<(), String> {
    let repo = Repository::init(root).map_err(|e| format!("git init failed: {e}"))?;

    let mut index = repo.index().map_err(|e| e.to_string())?;
    // DEFAULT (not FORCE) means ignored paths in .gitignore are skipped.
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|e| e.to_string())?;
    index.write().map_err(|e| e.to_string())?;

    let tree = repo
        .find_tree(index.write_tree().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let sig = signature(&repo)?;
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
        .map_err(|e| format!("git commit failed: {e}"))?;
    Ok(())
}

/// Host portion of a git URL (`github.com`, `gitlab.example.edu`), used to look up a stored token.
/// Tolerant of scheme, `user@`, an SSH `host:path`, and a port.
fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let rest = rest.rsplit('@').next().unwrap_or(rest);
    rest.split(['/', ':']).next().unwrap_or("").to_string()
}

/// libgit2 credentials callback: authenticate HTTPS with the token stored for the URL's host.
///
/// Only invoked when the server actually demands auth (a public clone never reaches here), so a
/// missing token surfaces as "authentication required" for a private repo rather than breaking the
/// public path. The username differs by provider — GitHub wants `x-access-token`, GitLab `oauth2` —
/// with the token as the password either way.
fn credentials(url: &str, _username: Option<&str>, _allowed: CredentialType) -> Result<Cred, GitError> {
    let host = host_of(url);
    match crate::auth::token_for_host(&host) {
        Some(token) => {
            let username = if host.contains("github") { "x-access-token" } else { "oauth2" };
            Cred::userpass_plaintext(username, &token)
        }
        None => Err(GitError::from_str(
            "no stored credentials for this host — sign in under Settings → Accounts",
        )),
    }
}

/// Clone `url` into `dest` (which must not yet exist). HTTPS remotes only.
///
/// Runs through the [`credentials`] callback, so a private repo the user is signed in to clones just
/// like a public one; a public repo never triggers the callback at all.
pub fn clone(url: &str, dest: &Path) -> Result<(), String> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(credentials);
    let mut fetch = git2::FetchOptions::new();
    fetch.remote_callbacks(callbacks);

    git2::build::RepoBuilder::new()
        .fetch_options(fetch)
        .clone(url, dest)
        .map_err(|e| format!("clone failed: {e}"))?;
    Ok(())
}

/// The `origin` remote URL, if the repo has one.
pub fn origin_url(root: &Path) -> Option<String> {
    let repo = Repository::open(root).ok()?;
    repo.find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(str::to_owned))
}

/// Point remote `name` at `url`, creating it if it does not yet exist.
pub fn set_remote(root: &Path, name: &str, url: &str) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;
    if repo.find_remote(name).is_ok() {
        repo.remote_set_url(name, url).map_err(|e| e.to_string())?;
    } else {
        repo.remote(name, url).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Push the current branch to `origin`, authenticating via the [`credentials`] callback.
///
/// Used to publish an authored collection. Fails clearly if the repo has no commits yet (nothing to
/// publish) or no `origin` (nowhere to publish to) — both of which the publish flow sets up first.
pub fn push(root: &Path) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;
    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(str::to_owned))
        .ok_or("nothing to publish yet — the collection has no commits")?;
    let mut remote = repo
        .find_remote("origin")
        .map_err(|e| format!("no 'origin' remote to push to: {e}"))?;

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(credentials);
    let mut opts = PushOptions::new();
    opts.remote_callbacks(callbacks);

    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    remote
        .push(&[refspec.as_str()], Some(&mut opts))
        .map_err(|e| format!("push failed: {e}"))
}

/// Stage everything and commit it onto the current HEAD.
///
/// Unlike [`init`], which creates the very first commit, this commits *onto* an existing history —
/// it is how a collection records each lab as it is added. Honours `.gitignore`, uses the machine's
/// git identity (falling back to a Hub one), and parents the new commit on the current HEAD so the
/// history is linear rather than a second root.
pub fn commit_all(root: &Path, message: &str) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;

    let mut index = repo.index().map_err(|e| e.to_string())?;
    // add_all treats a submodule path as a gitlink (the commit it points at), not its contents —
    // so this stages the updated .gitmodules and manifest without descending into the lab repo.
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|e| e.to_string())?;
    index.write().map_err(|e| e.to_string())?;

    let tree = repo
        .find_tree(index.write_tree().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let sig = signature(&repo)?;
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|e| format!("git commit failed: {e}"))?;
    Ok(())
}

/// Add `url` as a git submodule at `path` (relative to `root`), cloning it into place.
///
/// This is the real `git submodule add`: it writes the `.gitmodules` entry, clones the repo and
/// stages both the gitlink and `.gitmodules`. The caller commits afterwards (see [`commit_all`]).
/// Network-bound — the clone reaches out to the remote — so it can fail like any clone.
pub fn submodule_add(root: &Path, url: &str, path: &str) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;
    let mut submodule = repo
        .submodule(url, Path::new(path), true)
        .map_err(|e| format!("could not add submodule {path}: {e}"))?;

    // Authenticate the clone the same way [`clone`]/[`push`] do, so a just-published private repo
    // (or any private project) checks out rather than failing on credentials. A public repo never
    // reaches the callback, exactly as before.
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(credentials);
    let mut fetch = git2::FetchOptions::new();
    fetch.remote_callbacks(callbacks);
    let mut opts = git2::SubmoduleUpdateOptions::new();
    opts.fetch(fetch);

    // Clones the project into <root>/<path>; the returned repo handle is dropped once it's on disk.
    submodule
        .clone(Some(&mut opts))
        .map_err(|e| format!("could not clone {url}: {e}"))?;
    submodule
        .add_finalize()
        .map_err(|e| format!("could not finalize submodule {path}: {e}"))?;
    Ok(())
}

/// Remove the submodule at `path` (relative to `root`): its gitlink, working tree, `.gitmodules`
/// entry, `.git/config` section and stored `.git/modules/<path>` dir. libgit2 has no submodule
/// removal, so this undoes by hand what [`submodule_add`] set up. The caller commits afterwards.
///
/// Best-effort on the filesystem bits — a missing working tree or module dir is not an error, so a
/// half-removed submodule can still be cleaned up rather than wedging the collection.
pub fn submodule_remove(root: &Path, path: &str) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;

    // Drop the gitlink from the index so the removal is staged.
    let mut index = repo.index().map_err(|e| e.to_string())?;
    let _ = index.remove_path(Path::new(path));
    index.write().map_err(|e| e.to_string())?;

    // Strip the `submodule.<name>` section from .git/config (name == path, as `submodule_add` sets).
    if let Ok(mut config) = repo.config() {
        let _ = config.remove_multivar(&format!("submodule.{path}.url"), ".*");
        let _ = config.remove_multivar(&format!("submodule.{path}.path"), ".*");
        let _ = config.remove_multivar(&format!("submodule.{path}.active"), ".*");
    }

    // Strip the matching `[submodule "<path>"]` block from .gitmodules.
    let gitmodules = root.join(".gitmodules");
    if gitmodules.is_file() {
        remove_gitmodules_section(&gitmodules, path)?;
    }

    // Delete the checked-out working tree and the submodule's stored git dir.
    let _ = std::fs::remove_dir_all(root.join(path));
    let _ = std::fs::remove_dir_all(root.join(".git").join("modules").join(path));
    Ok(())
}

/// Rewrite `.gitmodules` without the `[submodule "<name>"]` block — the section header and every
/// line under it up to the next `[section]` or end of file. Deletes the file entirely if that
/// leaves it empty, so no stray empty `.gitmodules` is committed.
fn remove_gitmodules_section(file: &Path, name: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let header = format!("[submodule \"{name}\"]");
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // A new section starts: skip only the one whose header matches.
            skipping = trimmed == header;
        }
        if !skipping {
            kept.push(line);
        }
    }
    if kept.iter().all(|l| l.trim().is_empty()) {
        std::fs::remove_file(file).map_err(|e| e.to_string())
    } else {
        let mut out = kept.join("\n");
        out.push('\n');
        std::fs::write(file, out).map_err(|e| e.to_string())
    }
}

/// Read a project's git status, or `None` if it is not a git repository.
pub fn info(root: &Path) -> Option<GitInfo> {
    let repo = Repository::open(root).ok()?;
    let branch = repo.head().ok().and_then(|h| h.shorthand().map(str::to_owned));
    let remote = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(str::to_owned));
    Some(GitInfo { branch, dirty: is_dirty(&repo), remote })
}

/// The project folder name a clone of `url` will land in: the last path segment, without `.git`.
pub fn repo_name_from_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .to_string()
}

fn is_dirty(repo: &Repository) -> bool {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    repo.statuses(Some(&mut opts)).map(|s| !s.is_empty()).unwrap_or(false)
}

fn signature(repo: &Repository) -> Result<Signature<'static>, String> {
    repo.signature()
        .or_else(|_| Signature::now("Koral Hub", "hub@koral.dev"))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("koral-git-test-{tag}-{n}"))
    }

    #[test]
    fn init_makes_a_committed_repo_with_status() {
        let root = scratch("init");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("koral.json"), "{}").unwrap();

        init(&root).unwrap();

        let info = info(&root).expect("should be a repo after init");
        assert!(info.branch.is_some(), "initial commit gives HEAD a branch");
        assert!(!info.dirty, "everything was committed, so the tree is clean");
        assert!(info.remote.is_none(), "a fresh init has no origin");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn info_is_none_for_a_plain_folder() {
        let root = scratch("plain");
        std::fs::create_dir_all(&root).unwrap();
        assert!(info(&root).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repo_name_strips_git_suffix_and_path() {
        assert_eq!(repo_name_from_url("https://github.com/user/my-proj.git"), "my-proj");
        assert_eq!(repo_name_from_url("https://github.com/user/my-proj"), "my-proj");
        assert_eq!(repo_name_from_url("https://github.com/user/my-proj/"), "my-proj");
        assert_eq!(repo_name_from_url("git@github.com:user/my-proj.git"), "my-proj");
    }

    /// Confirms HTTPS transport actually works with the vendored OpenSSL build. Network-bound, so
    /// it's ignored by default; run with `cargo test -- --ignored` to exercise it.
    #[test]
    #[ignore]
    fn clone_over_https_works() {
        let dest = scratch("clone");
        clone("https://github.com/octocat/Hello-World.git", &dest).unwrap();
        assert!(dest.join(".git").is_dir(), "clone should produce a working tree");
        std::fs::remove_dir_all(&dest).ok();
    }
}
