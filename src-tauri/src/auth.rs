//! Sign-in and stored credentials for GitHub and GitLab, via the OAuth 2.0 **Device Authorization
//! Grant** — the "go to this URL and enter this code" flow the `gh` CLI uses.
//!
//! Why device flow rather than a pasted token: the Hub is usually launched from a desktop shortcut,
//! with no browser redirect to catch and no terminal to paste into. Device flow needs neither — the
//! user authorizes in whatever browser they already have, and the Hub polls for the result.
//!
//! Tokens are stored on the machine (see [`paths::accounts_file`]) and used for three things: the
//! git credentials callback (private clones, in `git`), pushing an authored collection, and creating
//! the remote repository to push it to ([`create_remote_repo`]). They never travel with a project.
//!
//! ## Configuration
//! Device flow requires a registered OAuth application per provider; its **Client ID** is public
//! (no client secret is used in device flow), so it is baked into the Hub. Register the apps and put
//! the IDs in [`GITHUB_CLIENT_ID`] / [`GITLAB_CLIENT_ID`] below — or override at runtime with the
//! `KORAL_GITHUB_CLIENT_ID` / `KORAL_GITLAB_CLIENT_ID` environment variables while testing. Until an
//! ID is set, that provider's sign-in returns a clear "not configured" error rather than failing
//! cryptically.
//!
//!  - GitHub: Settings → Developer settings → OAuth Apps → New. **Enable device flow.** No callback
//!    URL is needed for device flow. Users grant the `repo` scope.
//!  - GitLab: Settings → Applications. Scopes `api read_repository write_repository`; the app must be
//!    a public client (no secret) with the device grant enabled (gitlab.com, or a self-hosted host).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::paths;

const USER_AGENT: &str = "KoralHub";

/// OAuth App Client IDs. **Public** values — device flow uses no client secret, so these are safe to
/// compile into a widely-distributed build (the same way the `gh` CLI ships its client ID in the
/// open). A maintainer registers one OAuth app per provider and sets these once at build time; end
/// users never see or touch them. Override with the matching `KORAL_*_CLIENT_ID` env var while
/// testing. Empty means "sign-in not configured".
const GITHUB_CLIENT_ID: &str = "Ov23lieeME0YKmQfAYHr";
const GITLAB_CLIENT_ID: &str = "";

/// Scopes requested. `repo` lets the Hub clone and push private repos on GitHub; the GitLab set is
/// its equivalent plus `api` for creating the project to publish into.
const GITHUB_SCOPE: &str = "repo";
const GITLAB_SCOPE: &str = "api read_repository write_repository";

/// The OAuth device-grant type string, per RFC 8628. Both providers expect exactly this.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    GitHub,
    GitLab,
}

impl Provider {
    fn label(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::GitLab => "GitLab",
        }
    }

    /// The configured Client ID, from the env override first, then the baked-in const.
    fn client_id(self) -> Option<String> {
        let (var, konst) = match self {
            Provider::GitHub => ("KORAL_GITHUB_CLIENT_ID", GITHUB_CLIENT_ID),
            Provider::GitLab => ("KORAL_GITLAB_CLIENT_ID", GITLAB_CLIENT_ID),
        };
        std::env::var(var)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| (!konst.is_empty()).then(|| konst.to_string()))
    }

    fn scope(self) -> &'static str {
        match self {
            Provider::GitHub => GITHUB_SCOPE,
            Provider::GitLab => GITLAB_SCOPE,
        }
    }

    /// Endpoint that issues a device+user code. GitHub is fixed; GitLab is per-host (self-hosted).
    fn device_code_url(self, host: &str) -> String {
        match self {
            Provider::GitHub => "https://github.com/login/device/code".into(),
            Provider::GitLab => format!("https://{host}/oauth/authorize_device"),
        }
    }

    /// Endpoint polled to exchange the device code for an access token.
    fn token_url(self, host: &str) -> String {
        match self {
            Provider::GitHub => "https://github.com/login/oauth/access_token".into(),
            Provider::GitLab => format!("https://{host}/oauth/token"),
        }
    }

    /// REST API base, used to read the signed-in username and create repositories.
    fn api_base(self, host: &str) -> String {
        match self {
            Provider::GitHub => "https://api.github.com".into(),
            Provider::GitLab => format!("https://{host}/api/v4"),
        }
    }
}

/// Normalise the host for an account. GitHub is always `github.com`; GitLab accepts a custom host
/// (self-hosted), defaulting to `gitlab.com`, with any scheme/trailing slash stripped.
fn normalize_host(provider: Provider, host: Option<String>) -> String {
    match provider {
        Provider::GitHub => "github.com".to_string(),
        Provider::GitLab => {
            let h = host.unwrap_or_default();
            let h = h
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .trim();
            if h.is_empty() { "gitlab.com".to_string() } else { h.to_string() }
        }
    }
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))
}

// --- Device flow ------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    /// GitLab (and increasingly GitHub) return a URL with the code pre-filled.
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    /// Present while pending (`authorization_pending`, `slow_down`) or on failure.
    #[serde(default)]
    error: Option<String>,
}

/// What the UI shows the user so they can authorize: the code and where to type it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    /// Same URL with the code embedded, when the provider supplies it — nicer to open directly.
    pub verification_uri_complete: Option<String>,
}

/// Everything the background poll needs to finish a sign-in. Not sent to the UI.
#[derive(Debug)]
pub struct PollContext {
    provider: Provider,
    host: String,
    client_id: String,
    device_code: String,
    interval: u64,
    expires_in: u64,
}

/// Begin a sign-in: request a device code and return what to show the user, plus the context the
/// caller then hands to [`poll_device_login`] (typically on a background thread).
pub fn start_device_login(
    provider: Provider,
    host: Option<String>,
) -> Result<(DeviceLogin, PollContext), String> {
    let host = normalize_host(provider, host);
    let client_id = provider.client_id().ok_or_else(|| {
        format!(
            "{} sign-in isn't configured yet — a maintainer needs to register an OAuth app and set \
             its Client ID in the Hub",
            provider.label()
        )
    })?;

    let resp: DeviceCodeResponse = http()?
        .post(provider.device_code_url(&host))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("client_id", client_id.as_str()), ("scope", provider.scope())])
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("could not start {} sign-in: {e}", provider.label()))?;

    let login = DeviceLogin {
        user_code: resp.user_code,
        verification_uri: resp.verification_uri,
        verification_uri_complete: resp.verification_uri_complete,
    };
    let ctx = PollContext {
        provider,
        host,
        client_id,
        device_code: resp.device_code,
        // Providers may send 0; fall back to the RFC's suggested 5s floor and a sane expiry.
        interval: resp.interval.max(5),
        expires_in: resp.expires_in.max(300),
    };
    Ok((login, ctx))
}

/// Poll until the user authorizes (or the code expires), then save the account and return its view.
///
/// Blocks — sleeping `interval` seconds between polls — so it belongs on a background thread. Honours
/// the provider's `slow_down` by backing off, and gives up cleanly at `expires_in`.
pub fn poll_device_login(ctx: PollContext) -> Result<AccountView, String> {
    let client = http()?;
    let deadline = Instant::now() + Duration::from_secs(ctx.expires_in);
    let mut interval = ctx.interval;

    loop {
        if Instant::now() >= deadline {
            return Err("sign-in timed out — start it again".into());
        }
        std::thread::sleep(Duration::from_secs(interval));

        // Don't `error_for_status` here: GitLab reports "still pending" as HTTP 400 with a JSON
        // body, so the body is the source of truth for both providers.
        let body: TokenResponse = client
            .post(ctx.provider.token_url(&ctx.host))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", ctx.client_id.as_str()),
                ("device_code", ctx.device_code.as_str()),
                ("grant_type", DEVICE_GRANT),
            ])
            .send()
            .and_then(|r| r.json())
            .map_err(|e| format!("sign-in check failed: {e}"))?;

        if let Some(token) = body.access_token {
            let username = fetch_username(ctx.provider, &ctx.host, &token)?;
            let account = Account { provider: ctx.provider, host: ctx.host.clone(), username, token };
            save_account(&account)?;
            return Ok(AccountView::from(&account));
        }
        match body.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => interval += 5,
            Some(other) => return Err(format!("sign-in failed: {other}")),
            None => return Err("sign-in failed: the provider returned no token and no error".into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    /// GitHub uses `login`; GitLab uses `username`. Whichever is present is the display name.
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

fn fetch_username(provider: Provider, host: &str, token: &str) -> Result<String, String> {
    let resp: UserResponse = http()?
        .get(format!("{}/user", provider.api_base(host)))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("signed in, but could not read the account name: {e}"))?;
    Ok(resp.login.or(resp.username).unwrap_or_else(|| "unknown".into()))
}

// --- Opening the verification page --------------------------------------------------------

/// Open `url` in the user's default browser, so the device-flow dialog's "Open page" button lands
/// the user on the provider's verification page without copy-paste. Purely local — no network — and
/// best-effort: if it can't launch a browser, the dialog still shows the URL to open by hand.
pub fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // The empty "" is `start`'s window-title argument; without it a quoted URL is swallowed.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open a browser: {e}"))
}

// --- Account store (per machine, with tokens) -------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub provider: Provider,
    pub host: String,
    pub username: String,
    /// OAuth access token. Sensitive; see [`paths::accounts_file`].
    pub token: String,
}

/// An account without its token, for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub provider: Provider,
    pub host: String,
    pub username: String,
}

impl From<&Account> for AccountView {
    fn from(a: &Account) -> Self {
        AccountView { provider: a.provider, host: a.host.clone(), username: a.username.clone() }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct AccountStore {
    accounts: Vec<Account>,
}

fn load_store() -> AccountStore {
    std::fs::read_to_string(paths::accounts_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_store(store: &AccountStore) -> Result<(), String> {
    let file = paths::accounts_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(&file, text).map_err(|e| format!("failed to write {}: {e}", file.display()))?;

    // Tokens are secrets — keep the file readable only by its owner. Best-effort: a filesystem that
    // can't represent Unix modes (rare for the data dir) shouldn't fail the sign-in.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Save (replacing any existing account for the same provider+host).
fn save_account(account: &Account) -> Result<(), String> {
    let mut store = load_store();
    store
        .accounts
        .retain(|a| !(a.provider == account.provider && a.host == account.host));
    store.accounts.push(account.clone());
    save_store(&store)
}

/// Every signed-in account, without tokens.
pub fn accounts() -> Vec<AccountView> {
    load_store().accounts.iter().map(AccountView::from).collect()
}

/// Sign out of one account.
pub fn sign_out(provider: Provider, host: &str) -> Result<(), String> {
    let mut store = load_store();
    store
        .accounts
        .retain(|a| !(a.provider == provider && a.host == host));
    save_store(&store)
}

/// The token to authenticate git operations against `host`, if signed in there. Used by the git
/// credentials callback, so a plain host string (`github.com`, `gitlab.example.edu`) is the key.
pub fn token_for_host(host: &str) -> Option<String> {
    load_store()
        .accounts
        .into_iter()
        .find(|a| a.host == host)
        .map(|a| a.token)
}

/// The full account for `host`, for operations that also need the provider and username (publish).
pub fn account_for_host(host: &str) -> Option<Account> {
    load_store().accounts.into_iter().find(|a| a.host == host)
}

/// Split a git URL into its host and first path segment (the owner / namespace). Handles the HTTPS
/// (`https://github.com/owner/repo.git`) and SSH (`git@github.com:owner/repo.git`) forms. `None` for
/// anything that doesn't carry a host and an owner.
fn host_and_owner(url: &str) -> Option<(String, String)> {
    let rest = url.trim();
    let rest = rest.split("://").nth(1).unwrap_or(rest);
    // Drop an optional `user@` (the SSH form's `git@`), keeping whatever follows the last `@`.
    let rest = rest.rsplitn(2, '@').next().unwrap_or(rest);
    let (host, tail) = rest.split_once(|c| c == '/' || c == ':')?;
    let owner = tail.trim_start_matches('/').split('/').next().unwrap_or("");
    if host.is_empty() || owner.is_empty() {
        return None;
    }
    Some((host.to_string(), owner.to_string()))
}

/// True when a signed-in account owns the repository at `url` — some account's host matches the
/// URL's host and its username matches the repo's owner. Lets a download tell the user's own repo
/// (keep it as a live clone they can push to) from someone else's (fork into a copy they own).
pub fn signed_in_owns(url: &str) -> bool {
    match host_and_owner(url) {
        Some((host, owner)) => load_store().accounts.iter().any(|a| {
            a.host.eq_ignore_ascii_case(&host) && a.username.eq_ignore_ascii_case(&owner)
        }),
        None => false,
    }
}

// --- Creating a repository to publish into ----------------------------------------------

/// Create a new remote repository under the signed-in account and return its HTTPS git URL.
///
/// This is what turns "publish" into one action: the Hub makes the empty repo, then pushes into it.
/// A name that already exists is reported as an error rather than reused — publishing again uses the
/// collection's existing `origin` instead of coming back through here (see `collection`/`commands`).
pub fn create_remote_repo(
    account: &Account,
    name: &str,
    description: &str,
    private: bool,
) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("enter a repository name".into());
    }
    let client = http()?;

    let (url, response) = match account.provider {
        Provider::GitHub => {
            let body = serde_json::json!({
                "name": name,
                "description": description,
                "private": private,
                "auto_init": false,
            });
            let resp = client
                .post("https://api.github.com/user/repos")
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", account.token))
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .json(&body)
                .send()
                .map_err(|e| e.to_string())?;
            ("clone_url", resp)
        }
        Provider::GitLab => {
            let body = serde_json::json!({
                "name": name,
                "description": description,
                "visibility": if private { "private" } else { "public" },
            });
            let resp = client
                .post(format!("https://{}/api/v4/projects", account.host))
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", account.token))
                .json(&body)
                .send()
                .map_err(|e| e.to_string())?;
            ("http_url_to_repo", resp)
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().unwrap_or_default();
        return Err(format!(
            "{} would not create '{name}' ({status}): {detail}",
            account.provider.label()
        ));
    }

    let json: serde_json::Value = response.json().map_err(|e| e.to_string())?;
    json.get(url)
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} did not return a repository URL", account.provider.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_host_is_fixed_but_gitlab_is_normalized() {
        assert_eq!(normalize_host(Provider::GitHub, Some("whatever".into())), "github.com");
        assert_eq!(normalize_host(Provider::GitLab, None), "gitlab.com");
        assert_eq!(normalize_host(Provider::GitLab, Some("  ".into())), "gitlab.com");
        assert_eq!(
            normalize_host(Provider::GitLab, Some("https://gitlab.example.edu/".into())),
            "gitlab.example.edu"
        );
    }

    #[test]
    fn provider_serializes_lowercase_for_the_frontend() {
        assert_eq!(serde_json::to_string(&Provider::GitHub).unwrap(), "\"github\"");
        assert_eq!(
            serde_json::from_str::<Provider>("\"gitlab\"").unwrap(),
            Provider::GitLab
        );
    }

    #[test]
    fn host_and_owner_parses_https_and_ssh_forms() {
        assert_eq!(
            host_and_owner("https://github.com/octocat/Hello-World.git"),
            Some(("github.com".into(), "octocat".into()))
        );
        assert_eq!(
            host_and_owner("git@github.com:octocat/Hello-World.git"),
            Some(("github.com".into(), "octocat".into()))
        );
        assert_eq!(
            host_and_owner("https://gitlab.example.edu/group/sub/repo"),
            Some(("gitlab.example.edu".into(), "group".into()))
        );
        assert_eq!(host_and_owner("not-a-url"), None);
    }

    #[test]
    fn unconfigured_provider_reports_a_clear_error() {
        // With no env override and empty consts, starting a login fails with guidance, not a panic.
        if Provider::GitHub.client_id().is_none() {
            let err = start_device_login(Provider::GitHub, None).unwrap_err();
            assert!(err.contains("isn't configured"), "{err}");
        }
    }
}
