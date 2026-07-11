//! Native git via libgit2 (vendored, statically linked) — no system `git` binary required.
//!
//! Three jobs, all HTTPS-only:
//!  - [`init`]  — turn a freshly scaffolded project into a repo with one initial commit,
//!  - [`clone`] — pull a project down from a remote when importing,
//!  - [`info`]  — read the little bit of status the project cards show (branch, dirty, remote).

use std::path::Path;

use git2::{IndexAddOption, Repository, Signature, StatusOptions};
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

/// Clone `url` into `dest` (which must not yet exist). HTTPS remotes only.
pub fn clone(url: &str, dest: &Path) -> Result<(), String> {
    Repository::clone(url, dest).map_err(|e| format!("clone failed: {e}"))?;
    Ok(())
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
