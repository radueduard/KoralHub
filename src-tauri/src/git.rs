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

/// Callbacks every network operation shares: authenticate HTTPS with the stored token.
///
/// A fresh set per call because libgit2 takes ownership of them, and one operation's callbacks
/// cannot be handed to the next.
fn remote_callbacks() -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(credentials);
    callbacks
}

/// Clone `url` into `dest` (which must not yet exist). HTTPS remotes only.
///
/// Runs through the [`credentials`] callback, so a private repo the user is signed in to clones just
/// like a public one; a public repo never triggers the callback at all.
pub fn clone(url: &str, dest: &Path) -> Result<(), String> {
    let mut fetch = git2::FetchOptions::new();
    fetch.remote_callbacks(remote_callbacks());

    git2::build::RepoBuilder::new()
        .fetch_options(fetch)
        .clone(url, dest)
        .map_err(|e| format!("clone failed: {e}"))?;
    Ok(())
}

/// Clone `url` into `dest`, taking only the tip commit of its default branch when the transport
/// allows it.
///
/// For repositories the Hub treats as *content* rather than as a user's work: the vcpkg port tree,
/// which is a gigabyte of history nobody here will ever read. `update_from_origin` keeps working on
/// a shallow clone, since it force-resets onto whatever the next fetch brings rather than looking
/// for a merge base.
///
/// **Falls back to a full clone** when the depth is refused. Not every libgit2 transport implements
/// shallow fetch — the local one flatly rejects it ("shallow fetch is not supported by the local
/// transport") — and a checkout that costs more disk is a far better outcome than no vcpkg at all.
/// The retry is cheap: a refused depth fails during negotiation, before any objects are
/// transferred.
pub fn clone_shallow(url: &str, dest: &Path) -> Result<(), String> {
    let attempt = |depth: i32| {
        let mut fetch = git2::FetchOptions::new();
        fetch.remote_callbacks(remote_callbacks());
        if depth > 0 {
            fetch.depth(depth);
        }
        // A repo with thousands of tags would otherwise spend most of the clone on them.
        fetch.download_tags(git2::AutotagOption::None);
        git2::build::RepoBuilder::new()
            .fetch_options(fetch)
            .clone(url, dest)
            .map(|_| ())
    };

    if attempt(1).is_ok() {
        return Ok(());
    }
    // A failed clone can leave a partial directory behind, and libgit2 refuses to clone into a
    // non-empty one — so clear it before the retry rather than failing twice for two reasons.
    let _ = std::fs::remove_dir_all(dest);
    attempt(0).map_err(|e| format!("clone failed: {e}"))
}

/// What one [`update_from_origin`] did, so the UI can say whether anything actually moved.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReport {
    pub branch: String,
    /// Short commit id before and after. `from` is empty for a checkout that had no commit yet.
    pub from: String,
    pub to: String,
    /// False when the tree was already on the commit origin has — "already up to date".
    pub changed: bool,
}

/// Fetch `origin` and force this checkout onto it. **Overwrite, always.**
///
/// There is no merge and no rebase: the remote wins outright, so local commits on the branch and
/// local edits to tracked files are discarded. That is the whole contract — a lab or a shared
/// project is upstream's copy, and an update that could stop half way through a conflict would be
/// useless to someone who just wants the current version. Untracked files the remote knows nothing
/// about are left alone; a file the remote *does* carry is overwritten by the checkout.
///
/// Detached or unborn HEAD is put back onto the branch rather than left behind, and a branch this
/// checkout has never had is created — so a repository in any of the states an interrupted clone or
/// a hand-checked-out tag leaves behind ends up on origin's branch.
pub fn update_from_origin(root: &Path) -> Result<UpdateReport, String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;
    let mut remote = repo.find_remote("origin").map_err(|_| {
        "this has no 'origin' remote, so there is nothing to update from. Projects downloaded \
         from someone else's collection are deliberately cut loose from upstream — save yours to \
         git to give it a remote of its own."
            .to_string()
    })?;

    // Which branch to follow: the one checked out here, or origin's own default when HEAD is not
    // on a branch at all.
    let checked_out = repo
        .head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_owned));
    let branch = match &checked_out {
        Some(branch) => branch.clone(),
        None => default_branch(&mut remote)?,
    };

    let mut fetch = git2::FetchOptions::new();
    fetch.remote_callbacks(remote_callbacks());
    fetch.download_tags(git2::AutotagOption::None);
    // Forced (`+`) because a shallow clone — or a remote that was rewritten — has no fast-forward
    // to offer, and this is an overwrite either way.
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    remote
        .fetch(&[refspec.as_str()], Some(&mut fetch), None)
        .map_err(|e| format!("fetch failed: {e}"))?;

    let target = repo
        .find_reference(&format!("refs/remotes/origin/{branch}"))
        .map_err(|_| format!("origin has no branch '{branch}'"))?
        .peel_to_commit()
        .map_err(|e| format!("origin/{branch} does not point at a commit: {e}"))?;

    let before = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).map(|c| c.id());

    // Put HEAD on the branch first, when it is not already — creating the branch if this checkout
    // never had it. Only in that case: libgit2 refuses to force-move a branch that *is* the current
    // HEAD, and it does not need to be moved that way, because the reset below moves it.
    if checked_out.as_deref() != Some(branch.as_str()) {
        repo.branch(&branch, &target, true)
            .map_err(|e| format!("could not move {branch} onto origin: {e}"))?;
        repo.set_head(&format!("refs/heads/{branch}"))
            .map_err(|e| format!("could not switch to {branch}: {e}"))?;
    }

    // Moves the current branch onto the target commit and forces the working tree to match it.
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.reset(target.as_object(), git2::ResetType::Hard, Some(&mut checkout))
        .map_err(|e| format!("could not check out origin/{branch}: {e}"))?;

    Ok(UpdateReport {
        changed: before != Some(target.id()),
        from: before.map(short_id).unwrap_or_default(),
        to: short_id(target.id()),
        branch,
    })
}

/// The branch `origin`'s HEAD points at, for a checkout that is not on a branch itself.
fn default_branch(remote: &mut git2::Remote) -> Result<String, String> {
    remote
        .connect_auth(git2::Direction::Fetch, Some(remote_callbacks()), None)
        .map_err(|e| format!("could not reach origin: {e}"))?;
    let head = remote
        .default_branch()
        .map_err(|e| format!("origin does not say which branch is its default: {e}"))?;
    let name = head
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("refs/heads/")
        .to_string();
    let _ = remote.disconnect();

    if name.is_empty() {
        return Err("could not work out which branch origin's HEAD points at".into());
    }
    Ok(name)
}

fn short_id(id: git2::Oid) -> String {
    id.to_string().chars().take(7).collect()
}

/// Bring every submodule to the commit the superproject now records, cloning any that are missing.
///
/// The other half of updating a collection: a fetch moves the gitlinks, and this is what makes the
/// checked-out entries match them. Forced, like [`update_from_origin`] — a submodule is upstream's
/// copy of a project, not somewhere to keep work.
pub fn update_submodules(root: &Path) -> Result<(), String> {
    let repo = Repository::open(root).map_err(|e| format!("not a git repository: {e}"))?;
    for mut submodule in repo.submodules().map_err(|e| e.to_string())? {
        let name = submodule.name().unwrap_or("?").to_string();

        let mut fetch = git2::FetchOptions::new();
        fetch.remote_callbacks(remote_callbacks());
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        let mut opts = git2::SubmoduleUpdateOptions::new();
        opts.fetch(fetch);
        opts.checkout(checkout);

        // `init = true` writes the .git/config entry for a submodule this clone has never checked
        // out, which is the state every entry is in right after a collection is cloned.
        submodule
            .update(true, Some(&mut opts))
            .map_err(|e| format!("could not update '{name}': {e}"))?;
    }
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

    /// The whole contract of "update": upstream wins, without exception.
    ///
    /// A student's checkout that has been edited, committed onto, and had files deleted still ends
    /// up byte-for-byte on origin's tip — because the alternative is an update that stops on a
    /// conflict, which is no use to someone who only wants the current version. What it must *not*
    /// touch is a file the remote knows nothing about.
    #[test]
    fn update_overwrites_local_work_but_leaves_untracked_files() {
        let base = scratch("update");
        let origin = base.join("origin");
        let clone_dir = base.join("clone");
        std::fs::create_dir_all(&origin).unwrap();

        // Upstream: one commit.
        std::fs::write(origin.join("koral.json"), r#"{"v":1}"#).unwrap();
        std::fs::write(origin.join("kept.txt"), "upstream v1").unwrap();
        init(&origin).unwrap();
        clone(origin.to_str().unwrap(), &clone_dir).unwrap();

        // The student diverges: edits a tracked file, deletes another, commits it, and leaves a
        // file of their own that upstream has never heard of.
        std::fs::write(clone_dir.join("koral.json"), "LOCAL EDIT").unwrap();
        std::fs::remove_file(clone_dir.join("kept.txt")).unwrap();
        commit_all(&clone_dir, "local work").unwrap();
        // Written after the commit, so it is genuinely untracked — the case that must survive.
        std::fs::write(clone_dir.join("notes.md"), "my own notes").unwrap();

        // Upstream moves on.
        std::fs::write(origin.join("koral.json"), r#"{"v":2}"#).unwrap();
        std::fs::write(origin.join("kept.txt"), "upstream v2").unwrap();
        commit_all(&origin, "upstream v2").unwrap();

        let report = update_from_origin(&clone_dir).unwrap();
        assert!(report.changed, "the tree moved, so it must be reported as changed");
        assert_ne!(report.from, report.to);

        assert_eq!(
            std::fs::read_to_string(clone_dir.join("koral.json")).unwrap(),
            r#"{"v":2}"#,
            "a locally edited tracked file is overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(clone_dir.join("kept.txt")).unwrap(),
            "upstream v2",
            "a locally deleted tracked file comes back"
        );
        assert_eq!(
            std::fs::read_to_string(clone_dir.join("notes.md")).unwrap(),
            "my own notes",
            "a file upstream does not carry is not the update's to delete"
        );
        // Nothing *tracked* is left modified: with the untracked file out of the way the tree is
        // clean, which is what "matches upstream" has to mean.
        std::fs::remove_file(clone_dir.join("notes.md")).unwrap();
        assert!(!info(&clone_dir).unwrap().dirty, "the tree matches HEAD once reset");

        // Running it again with nothing new upstream is a no-op that says so.
        let again = update_from_origin(&clone_dir).unwrap();
        assert!(!again.changed, "already up to date");
        assert_eq!(again.to, report.to);

        std::fs::remove_dir_all(&base).ok();
    }

    /// The vcpkg checkout is cloned with `depth 1` to keep a gigabyte of port history off every
    /// user's disk — but not every libgit2 transport implements shallow fetch, and the local one
    /// refuses outright. So the depth must be a *preference*, not a requirement: this exercises
    /// precisely the transport that rejects it, and the clone still has to succeed.
    ///
    /// That the depth is honoured where it *is* supported cannot be shown without a real HTTPS
    /// remote; what matters here is that a refusal costs disk rather than the feature.
    #[test]
    fn a_shallow_clone_falls_back_when_the_transport_refuses_the_depth() {
        let base = scratch("shallow");
        let origin = base.join("origin");
        std::fs::create_dir_all(&origin).unwrap();

        std::fs::write(origin.join("a.txt"), "1").unwrap();
        init(&origin).unwrap();
        for n in 2..=4 {
            std::fs::write(origin.join("a.txt"), n.to_string()).unwrap();
            commit_all(&origin, &format!("commit {n}")).unwrap();
        }

        let dest = base.join("checkout");
        clone_shallow(origin.to_str().unwrap(), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("a.txt")).unwrap(),
            "4",
            "the working tree must be the tip, however much history came with it"
        );

        // And it still updates afterwards: upstream moves, and the force-reset brings it over.
        std::fs::write(origin.join("a.txt"), "5").unwrap();
        commit_all(&origin, "commit 5").unwrap();
        assert!(update_from_origin(&dest).unwrap().changed);
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "5");

        std::fs::remove_dir_all(&base).ok();
    }

    /// A project downloaded from someone else's collection has its history dropped on purpose, so
    /// it has no remote to update from. That has to say so in those terms rather than failing
    /// somewhere inside libgit2.
    #[test]
    fn updating_something_with_no_origin_explains_itself() {
        let root = scratch("no-origin");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("koral.json"), "{}").unwrap();
        init(&root).unwrap();

        let err = update_from_origin(&root).unwrap_err();
        assert!(err.contains("no 'origin' remote"), "{err}");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Whether the depth is honoured on the transport that matters — HTTPS, which is how the vcpkg
    /// port tree actually arrives. The fallback above means a refusal is survivable, but it costs
    /// the user a gigabyte, so it is worth being able to check. Network-bound, hence ignored.
    #[test]
    #[ignore]
    fn a_shallow_clone_over_https_is_actually_shallow() {
        let dest = scratch("shallow-https");
        clone_shallow("https://github.com/octocat/Hello-World.git", &dest).unwrap();

        let repo = Repository::open(&dest).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_head().unwrap();
        assert_eq!(walk.count(), 1, "HTTPS should honour depth=1");

        std::fs::remove_dir_all(&dest).ok();
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
