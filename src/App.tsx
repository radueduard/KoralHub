import { createEffect, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { createStore } from "solid-js/store";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import logo from "./assets/logo.png";
import "./App.css";

// Mirrors the `RecentProject` DTO returned by the Rust commands.
// Scene = realtime app with an Update/Render loop and a window.
// Job   = single-dispatch headless app: Run() once to completion, then exit.
type Kind = "Scene" | "Job";

// Mirrors `GitInfo` — the bit of git status shown on a card. Null when the folder isn't a repo.
type GitInfo = {
  branch: string | null;
  dirty: boolean;
  remote: string | null;
};

type RecentProject = {
  name: string;
  path: string;
  color: [number, number, number];
  frameworkVersion: string;
  kind: Kind;
  git: GitInfo | null;
};

// Mirrors `AvailableFramework` — a release publishing an SDK for this platform.
type AvailableFramework = {
  version: string;
  tag: string;
  publishedAt: string;
  prerelease: boolean;
  /// Unpublished — only visible because this machine has a GitHub token.
  draft: boolean;
  assetName: string;
  assetSize: number;
  installed: boolean;
};

// Mirrors `InstalledFramework` — an SDK unpacked on this machine.
type InstalledFramework = {
  version: string;
  platform: string;
  path: string;
  sizeBytes: number;
};

// Mirrors `ProjectConfig` (koral.json). Only the fields the settings panel edits are spelled
// out; the rest ride along untouched so saving never drops data the Hub doesn't understand.
type ProjectConfig = {
  name: string;
  kind: Kind;
  rendering: {
    api: "Vulkan" | "OpenGL";
    window: {
      width: number;
      height: number;
      resizable: boolean;
      fullscreen: boolean;
      borderless: boolean;
      transparent: boolean;
      vsync: boolean;
    };
  };
  // Search lists, not single folders: a project can keep its own assets/ and also pull from a
  // shared library next door. Searched in order; the engine's built-in content comes after.
  paths: { assetDirectories: string[]; shaderDirectories: string[] };
  [key: string]: unknown;
};

// Mirrors `Settings` — the Hub's machine-local preferences. An empty string means "no preference";
// what that resolves to is reported separately as ResolvedDefaults.
type Settings = {
  projectLocation: string;
  defaultIde: string;
  defaultFrameworkVersion: string;
};

// What the empty settings currently fall back to — shown as placeholders, so a blank field is
// honest about what it will do instead of just looking unset.
type ResolvedDefaults = {
  projectLocation: string;
  ideId: string;
  frameworkVersion: string;
};

// Mirrors `Ide` — an editor found on this machine.
type Ide = {
  id: string;
  name: string;
  command: string;
};

type Finished = { success: boolean; error: string | null };
type InstallProgress = { version: string; downloaded: number; total: number };
type InstallFinished = { version: string; success: boolean; error: string | null };

function rgb([r, g, b]: [number, number, number]): string {
  return `rgb(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)})`;
}

function mb(bytes: number): string {
  return `${(bytes / 1_048_576).toFixed(1)} MB`;
}

// The name becomes a C++ class, its source filenames and the CMake project name (see the
// templates in project.rs), so anything that isn't a plain identifier would scaffold a
// project that cannot compile. Caught here rather than after the files are on disk.
const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_]*$/;

function nameProblem(name: string): string | null {
  if (!name) return null;
  if (!IDENTIFIER.test(name)) {
    return "Use letters, digits and underscores only; can't start with a digit.";
  }
  return null;
}

function joinPath(location: string, name: string): string {
  const sep = location.includes("\\") ? "\\" : "/";
  return `${location.replace(/[/\\]+$/, "")}${sep}${name}`;
}

// The folder a clone lands in — mirrors `git::repo_name_from_url` so the dialog's preview matches
// what the backend actually does: last path segment, without a trailing ".git".
function gitRepoName(url: string): string {
  const last = url.trim().replace(/\/+$/, "").split(/[/:]/).pop() ?? "";
  return last.replace(/\.git$/, "");
}

export default function App() {
  const [projects, { refetch }] = createResource<RecentProject[]>(() =>
    invoke("list_recent_projects"),
  );
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // Light/dark theme. Seeded from what the index.html boot script already applied (so this
  // matches what's on screen), then kept in sync with <html data-theme> and localStorage.
  type Theme = "dark" | "light";
  const [theme, setTheme] = createSignal<Theme>(
    document.documentElement.dataset.theme === "light" ? "light" : "dark",
  );
  createEffect(() => {
    document.documentElement.dataset.theme = theme();
    localStorage.setItem("koral-theme", theme());
  });
  const toggleTheme = () => setTheme((t) => (t === "dark" ? "light" : "dark"));

  // Splash screen: a big centered logo shown while the app boots, faded out once the primary
  // content (the recent-projects list) has settled. A minimum on-screen time keeps it from
  // flickering on a fast load; a safety timeout in onMount guarantees it never traps the user if
  // a command hangs. Not gated on `available` — that hits the network and can be slow/offline.
  const [booting, setBooting] = createSignal(true);
  const bootStartedAt = Date.now();
  createEffect(() => {
    if (booting() && !projects.loading) {
      const remaining = Math.max(0, 650 - (Date.now() - bootStartedAt));
      setTimeout(() => setBooting(false), remaining);
    }
  });
  // Fade out and remove the static splash (rendered by index.html) once boot is done.
  createEffect(() => {
    if (!booting()) {
      const el = document.getElementById("boot-splash");
      if (el) {
        el.classList.add("hidden");
        setTimeout(() => el.remove(), 500);
      }
    }
  });

  // Custom window controls — the native title bar is turned off (decorations: false), so the
  // header below is the draggable top bar and these drive minimize / maximize / close. `maximized`
  // only exists to swap the maximize button's glyph for a restore one.
  const appWindow = getCurrentWindow();
  const [maximized, setMaximized] = createSignal(false);

  const [log, setLog] = createSignal<string>("");
  const [running, setRunning] = createSignal(false);

  // New-project dialog. `location` outlives the dialog so it remembers the last folder used.
  const [showCreate, setCreating] = createSignal(false);
  const [name, setName] = createSignal("");
  const [location, setLocation] = createSignal("");
  const [kind, setKind] = createSignal<Kind>("Scene");
  const problem = () => nameProblem(name().trim());
  const canCreate = () => !!name().trim() && !!location() && !problem() && !busy();

  // Import-from-git dialog. Shares the create dialog's `location` (same default folder), plus a URL.
  const [showImport, setImporting] = createSignal(false);
  const [gitUrl, setGitUrl] = createSignal("");
  const canImport = () => !!gitUrl().trim() && !!location() && !busy();

  // Installed SDKs are local and always readable. Available ones come off the network, so the
  // resource can reject — the UI distinguishes "no releases" from "GitHub unreachable".
  const [installed, { refetch: refetchInstalled }] = createResource<InstalledFramework[]>(() =>
    invoke("installed_frameworks"),
  );
  const [available, { refetch: refetchAvailable }] = createResource<AvailableFramework[]>(() =>
    invoke("available_frameworks"),
  );
  // version -> percent, present only while that version is downloading.
  const [progress, setProgress] = createSignal<Record<string, number>>({});

  // IDEs on this machine. Fixed for the session — nobody installs CLion mid-session.
  const [ides] = createResource<Ide[]>(() => invoke("installed_ides"));

  // Hub preferences, and what they currently resolve to. `defaults` refetches after a save, so an
  // empty preference always shows the value it actually falls back to rather than a blank.
  const [defaults, { refetch: refetchDefaults }] = createResource<ResolvedDefaults>(() =>
    invoke("resolved_defaults"),
  );
  const defaultIde = () => ides()?.find((i) => i.id === defaults()?.ideId);

  const [showSettings, setShowSettings] = createSignal(false);
  const [prefs, setPrefs] = createStore<{ s: Settings | null }>({ s: null });

  // Every version you could pin a new project to: what is installed, plus what GitHub offers.
  // Both, because pinning a version you have not downloaded yet is legitimate — the Hub fetches
  // it on the first build.
  const frameworkChoices = () => {
    const versions = new Map<string, boolean>(); // version -> already installed
    for (const f of installed() ?? []) versions.set(f.version, true);
    for (const f of available() ?? []) {
      if (!versions.has(f.version)) versions.set(f.version, f.installed);
    }
    return [...versions]
      .map(([version, isInstalled]) => ({ version, installed: isInstalled }))
      .sort((a, b) => b.version.localeCompare(a.version, undefined, { numeric: true }));
  };
  // Path of the project currently being opened, so only its buttons show the pending state.
  const [opening, setOpening] = createSignal<string | null>(null);

  // Remove-project confirmation: the project awaiting confirmation, and whether to erase its files.
  const [removing, setRemoving] = createSignal<RecentProject | null>(null);
  const [deleteFiles, setDeleteFiles] = createSignal(false);

  // Settings panel: which project is open, plus a working copy of its koral.json that is only
  // written back on Save — so Cancel genuinely discards.
  const [settingsPath, setSettingsPath] = createSignal<string | null>(null);
  const [draft, setDraft] = createStore<{ cfg: ProjectConfig | null }>({ cfg: null });

  // Live build output streamed from the Rust builder. onCleanup is registered
  // synchronously (outside the async body) so it reliably binds to this component's scope.
  const unlisten: UnlistenFn[] = [];
  onCleanup(() => unlisten.forEach((u) => u()));
  onMount(async () => {
    // Never let a hanging command trap the user behind the splash.
    setTimeout(() => setBooting(false), 4000);

    // Keep the maximize/restore glyph in sync with the actual window state.
    setMaximized(await appWindow.isMaximized());
    unlisten.push(
      await appWindow.onResized(async () => setMaximized(await appWindow.isMaximized())),
    );

    unlisten.push(
      await listen<string>("build-output", (e) => setLog((l) => l + e.payload)),
    );
    unlisten.push(
      await listen<Finished>("build-finished", (e) => {
        setRunning(false);
        setLog((l) =>
          e.payload.success
            ? l + "\n✓ done\n"
            : l + `\n✗ ${e.payload.error ?? "failed"}\n`,
        );
        refetch();
      }),
    );

    unlisten.push(
      await listen<InstallProgress>("framework-progress", (e) => {
        const { version, downloaded, total } = e.payload;
        // total === 0 means the server sent no Content-Length; keep the bar indeterminate
        // rather than showing a fake percentage.
        const percent = total > 0 ? Math.round((downloaded / total) * 100) : -1;
        setProgress((p) => ({ ...p, [version]: percent }));
      }),
    );

    unlisten.push(
      await listen<InstallFinished>("framework-finished", (e) => {
        const { version, success, error: err } = e.payload;
        setProgress(({ [version]: _dropped, ...rest }) => rest);
        if (!success) setError(err ?? `failed to install ${version}`);
        refetchInstalled();
        refetchAvailable();
      }),
    );
  });

  function installFramework(version: string) {
    setError(null);
    setProgress((p) => ({ ...p, [version]: 0 }));
    // Fire-and-forget: outcome arrives on `framework-finished`.
    invoke("install_framework", { version }).catch((e) => {
      setProgress(({ [version]: _dropped, ...rest }) => rest);
      setError(String(e));
    });
  }

  async function uninstallFramework(version: string) {
    setError(null);
    try {
      await invoke("uninstall_framework", { version });
      await Promise.all([refetchInstalled(), refetchAvailable()]);
    } catch (e) {
      setError(String(e));
    }
  }

  // Open the create dialog, seeding the location with ~/Koral (or whatever the last
  // creation used) so the common case is still one click away.
  async function openCreate() {
    setError(null);
    if (!location()) {
      try {
        setLocation(await invoke<string>("default_project_location"));
      } catch (e) {
        setError(String(e));
      }
    }
    setCreating(true);
  }

  async function browseLocation() {
    const picked = await open({
      directory: true,
      multiple: false,
      title: "Choose a folder for the new project",
      defaultPath: location() || undefined,
    });
    // `null` means the user cancelled — leave the current value alone.
    if (typeof picked === "string") setLocation(picked);
  }

  async function submitCreate(e: Event) {
    e.preventDefault();
    const trimmed = name().trim();
    if (!trimmed || !location() || nameProblem(trimmed)) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("create_project", {
        req: { location: location(), name: trimmed, kind: kind() },
      });
      await refetch();
      setCreating(false);
      setName("");
    } catch (e) {
      // Stay in the dialog so the name/location can be corrected — the most likely
      // failure is "a folder named X already exists".
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Open the import dialog, seeding the destination folder with the default location (same as the
  // create dialog) so an import is one paste-and-go in the common case.
  async function openImport() {
    setError(null);
    setGitUrl("");
    if (!location()) {
      try {
        setLocation(await invoke<string>("default_project_location"));
      } catch (e) {
        setError(String(e));
      }
    }
    setImporting(true);
  }

  async function submitImport(e: Event) {
    e.preventDefault();
    const url = gitUrl().trim();
    if (!url || !location()) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("import_project", { req: { url, location: location() } });
      await refetch();
      setImporting(false);
      setGitUrl("");
    } catch (e) {
      // Stay in the dialog so the URL can be fixed — the usual failures are a bad URL, a private
      // repo, or a clone that isn't a Koral project.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Removal is confirmed, and deleting the folder is a separate, explicit opt-in that resets each
  // time the dialog opens — it must never be sticky from a previous removal.
  async function confirmRemove(e: Event) {
    e.preventDefault();
    const project = removing();
    if (!project) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("remove_project", { path: project.path, deleteFiles: deleteFiles() });
      setRemoving(null);
      await refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function openSettingsPanel() {
    setError(null);
    try {
      setPrefs("s", await invoke<Settings>("settings"));
      setShowSettings(true);
    } catch (e) {
      setError(String(e));
    }
  }

  async function savePrefs(e: Event) {
    e.preventDefault();
    const s = prefs.s;
    if (!s) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("save_settings", { settings: s });
      setShowSettings(false);
      setPrefs("s", null);
      // The create dialog seeds its location from the old default, so drop it — otherwise the
      // next New Project would still open at the folder that was just changed.
      setLocation("");
      await refetchDefaults();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function browseDefaultLocation() {
    const picked = await open({
      directory: true,
      multiple: false,
      title: "Default folder for new projects",
      defaultPath: prefs.s?.projectLocation || defaults()?.projectLocation,
    });
    if (typeof picked === "string") setPrefs("s", "projectLocation", picked);
  }

  function askRemove(project: RecentProject) {
    setError(null);
    setDeleteFiles(false);
    setRemoving(project);
  }

  async function openSettings(path: string) {
    setError(null);
    try {
      setDraft("cfg", await invoke<ProjectConfig>("project_config", { path }));
      setSettingsPath(path);
    } catch (e) {
      setError(String(e));
    }
  }

  async function saveSettings(e: Event) {
    e.preventDefault();
    const path = settingsPath();
    const config = draft.cfg;
    if (!path || !config) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("save_project_config", { path, config });
      setSettingsPath(null);
      setDraft("cfg", null);
      await refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function closeSettings() {
    setSettingsPath(null);
    setDraft("cfg", null);
  }

  // Regenerates the IDE's build/run config before launching, so the editor is useful the moment
  // it opens. That means resolving (and possibly downloading) the SDK, hence the pending state.
  // `ideId` omitted → the backend uses the default from Settings.
  async function openInIde(path: string, ideId?: string) {
    setError(null);
    setOpening(path);
    try {
      // Explicit null, not undefined — an omitted key would not reach the Option<String> arg.
      await invoke("open_in_ide", { path, ideId: ideId ?? null });
    } catch (e) {
      setError(String(e));
    } finally {
      setOpening(null);
    }
  }

  async function runProject(path: string) {
    setError(null);
    setLog("");
    setRunning(true);
    try {
      await invoke("run_project", { path });
    } catch (e) {
      setRunning(false);
      setError(String(e));
    }
  }

  return (
    <div class="app">
      <header class="titlebar" data-tauri-drag-region>
        <div class="brand">
          <img class="brand-logo" src={logo} alt="" width="26" height="26" />
          <span class="brand-name">Koral&nbsp;Hub</span>
        </div>
        <div class="titlebar-actions">
          <button
            class="btn btn-ghost btn-icon"
            title={theme() === "dark" ? "Switch to light theme" : "Switch to dark theme"}
            onClick={toggleTheme}
          >
            {theme() === "dark" ? "☀" : "☾"}
          </button>
          <button class="btn btn-ghost" onClick={openSettingsPanel}>
            Settings
          </button>
          <button class="btn btn-ghost" disabled={busy()} onClick={openImport}>
            Import
          </button>
          <button class="btn btn-primary" disabled={busy()} onClick={openCreate}>
            New Project
          </button>

          {/* Native decorations are off, so we draw the window controls ourselves. */}
          <div class="window-controls">
            <button class="win-btn" title="Minimize" onClick={() => appWindow.minimize()}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                <line x1="1" y1="5" x2="9" y2="5" stroke="currentColor" stroke-width="1" />
              </svg>
            </button>
            <button
              class="win-btn"
              title={maximized() ? "Restore" : "Maximize"}
              onClick={() => appWindow.toggleMaximize()}
            >
              <Show
                when={maximized()}
                fallback={
                  <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                    <rect x="1" y="1" width="8" height="8" fill="none" stroke="currentColor" stroke-width="1" />
                  </svg>
                }
              >
                <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                  <rect x="1" y="3" width="6" height="6" fill="none" stroke="currentColor" stroke-width="1" />
                  <path d="M3 3 V1 H9 V7 H7" fill="none" stroke="currentColor" stroke-width="1" />
                </svg>
              </Show>
            </button>
            <button class="win-btn win-close" title="Close" onClick={() => appWindow.close()}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                <line x1="1" y1="1" x2="9" y2="9" stroke="currentColor" stroke-width="1" />
                <line x1="9" y1="1" x2="1" y2="9" stroke="currentColor" stroke-width="1" />
              </svg>
            </button>
          </div>
        </div>
      </header>

      <main class="content">
        <h1 class="section-title">Recent Projects</h1>

        <Show
          when={
            error() &&
            !showCreate() &&
            !showImport() &&
            !settingsPath() &&
            !removing() &&
            !showSettings()
          }
        >
          <p class="error">{error()}</p>
        </Show>

        <Show when={!projects.loading} fallback={<p class="muted">Loading…</p>}>
          <Show
            when={(projects()?.length ?? 0) > 0}
            fallback={
              <div class="empty">
                <p class="muted">No projects yet.</p>
                <p class="muted-sm">Create one to get started.</p>
              </div>
            }
          >
            <ul class="project-list">
              <For each={projects()}>
                {(p) => (
                  <li class="project-card">
                    <span class="project-swatch" style={{ "background-color": rgb(p.color) }}>
                      {p.name.charAt(0).toUpperCase()}
                    </span>
                    <span class="project-meta">
                      <span class="project-name">{p.name}</span>
                      <span class="project-path">{p.path}</span>
                    </span>
                    <span class="project-kind" classList={{ "kind-job": p.kind === "Job" }}>
                      {p.kind}
                    </span>
                    <span class="project-fw">koral {p.frameworkVersion}</span>
                    <Show when={p.git}>
                      <span
                        class="project-git"
                        title={
                          (p.git!.remote ?? "local git repository") +
                          (p.git!.dirty ? " · uncommitted changes" : "")
                        }
                      >
                        <svg
                          class="git-icon"
                          width="12"
                          height="12"
                          viewBox="0 0 12 12"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="1.2"
                          stroke-linecap="round"
                          aria-hidden="true"
                        >
                          <circle cx="3" cy="2.6" r="1.4" />
                          <circle cx="3" cy="9.4" r="1.4" />
                          <circle cx="9" cy="2.6" r="1.4" />
                          <path d="M3 4 v4" />
                          <path d="M9 4 v1 a3 3 0 0 1 -3 3 H3" />
                        </svg>
                        <span class="git-branch-name">{p.git!.branch ?? "detached"}</span>
                        <Show when={p.git!.dirty}>
                          <span class="git-dot">●</span>
                        </Show>
                      </span>
                    </Show>
                    <Show when={defaultIde()}>
                      <button
                        class="btn btn-ghost btn-ide"
                        title={`Open in ${defaultIde()!.name} (${defaultIde()!.command}) — change the default in Settings`}
                        disabled={opening() !== null}
                        onClick={() => openInIde(p.path)}
                      >
                        {opening() === p.path ? "Opening…" : `Open in ${defaultIde()!.name}`}
                      </button>
                    </Show>
                    <button
                      class="btn btn-icon"
                      title="Run settings"
                      onClick={() => openSettings(p.path)}
                    >
                      ⚙
                    </button>
                    <button
                      class="btn btn-play"
                      title="Build &amp; Run"
                      disabled={running()}
                      onClick={() => runProject(p.path)}
                    >
                      ▶
                    </button>
                    <button
                      class="btn btn-icon btn-danger"
                      title="Remove project"
                      onClick={() => askRemove(p)}
                    >
                      ✕
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>

        <h1 class="section-title">Framework</h1>

        <Show
          when={!available.error}
          fallback={
            <div class="empty">
              <p class="error">Could not reach GitHub: {String(available.error)}</p>
              <button class="btn btn-ghost" onClick={() => refetchAvailable()}>
                Retry
              </button>
            </div>
          }
        >
          <Show when={!available.loading} fallback={<p class="muted">Checking for releases…</p>}>
            <Show
              when={(available()?.length ?? 0) > 0}
              fallback={
                <div class="empty">
                  <p class="muted">No releases published for this platform yet.</p>
                </div>
              }
            >
              <ul class="fw-list">
                <For each={available()}>
                  {(fw) => {
                    const pct = () => progress()[fw.version];
                    const downloading = () => pct() !== undefined;
                    const local = () =>
                      installed()?.find((i) => i.version === fw.version);
                    return (
                      <li class="fw-card" classList={{ "fw-installed": fw.installed }}>
                        <span class="fw-meta">
                          <span class="fw-version">
                            koral {fw.version}
                            <Show when={fw.draft}>
                              <span class="fw-tag fw-tag-draft" title="Unpublished — visible only because you are signed in to GitHub">
                                draft
                              </span>
                            </Show>
                            <Show when={fw.prerelease && !fw.draft}>
                              <span class="fw-tag">pre-release</span>
                            </Show>
                          </span>
                          <span class="fw-sub">
                            {local()
                              ? `installed · ${mb(local()!.sizeBytes)} on disk`
                              : `${fw.assetName} · ${mb(fw.assetSize)}`}
                          </span>
                        </span>

                        <Show when={downloading()}>
                          <span class="fw-progress">
                            <progress
                              class="fw-bar"
                              max="100"
                              value={pct()! >= 0 ? pct()! : undefined}
                            />
                            <span class="muted-sm">
                              {pct()! >= 0 ? `${pct()}%` : "downloading…"}
                            </span>
                          </span>
                        </Show>

                        <Show when={!downloading()}>
                          <Show
                            when={fw.installed}
                            fallback={
                              <button
                                class="btn btn-primary"
                                onClick={() => installFramework(fw.version)}
                              >
                                Install
                              </button>
                            }
                          >
                            <button
                              class="btn btn-ghost"
                              title={`Delete ${local()?.path ?? fw.version}`}
                              onClick={() => uninstallFramework(fw.version)}
                            >
                              Uninstall
                            </button>
                          </Show>
                        </Show>
                      </li>
                    );
                  }}
                </For>
              </ul>
            </Show>
          </Show>
        </Show>
      </main>

      <Show when={showCreate()}>
        <div class="modal-scrim" onClick={() => setCreating(false)}>
          {/* Clicks inside the dialog must not reach the scrim's dismiss handler. */}
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitCreate}>
            <h2 class="modal-title">New Project</h2>

            <span class="field-label">Template</span>
            <div class="template-picker">
              <For
                each={
                  [
                    ["Scene", "Realtime app with a window and an Update/Render loop."],
                    ["Job", "Headless single dispatch: Run() once to completion, then exit."],
                  ] as const
                }
              >
                {([value, blurb]) => (
                  <button
                    type="button"
                    class="template-card"
                    classList={{ "template-active": kind() === value }}
                    onClick={() => setKind(value)}
                  >
                    <span class="template-name">{value}</span>
                    <span class="template-blurb">{blurb}</span>
                  </button>
                )}
              </For>
            </div>

            <label class="field">
              <span class="field-label">Name</span>
              <input
                class="input"
                value={name()}
                placeholder="MyProject"
                autofocus
                onInput={(e) => setName(e.currentTarget.value)}
              />
            </label>
            <Show when={problem()}>
              <p class="field-hint field-bad">{problem()}</p>
            </Show>

            <label class="field">
              <span class="field-label">Location</span>
              <span class="field-row">
                <input
                  class="input"
                  value={location()}
                  placeholder="~/Koral"
                  onInput={(e) => setLocation(e.currentTarget.value)}
                />
                <button type="button" class="btn btn-ghost" onClick={browseLocation}>
                  Browse…
                </button>
              </span>
            </label>

            <Show when={location() && name().trim() && !problem()}>
              <p class="field-hint">
                Creates <code>{joinPath(location(), name().trim())}</code>
              </p>
            </Show>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setCreating(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canCreate()}>
                {busy() ? "Creating…" : "Create"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={showImport()}>
        <div class="modal-scrim" onClick={() => setImporting(false)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitImport}>
            <h2 class="modal-title">Import from Git</h2>
            <p class="field-hint">
              Clone a Koral project from a git repository. The folder name comes from the repo, and
              it must contain a <code>koral.json</code>.
            </p>

            <label class="field">
              <span class="field-label">Repository URL</span>
              <input
                class="input"
                value={gitUrl()}
                placeholder="https://github.com/user/my-koral-project.git"
                autofocus
                onInput={(e) => setGitUrl(e.currentTarget.value)}
              />
            </label>

            <label class="field">
              <span class="field-label">Destination</span>
              <span class="field-row">
                <input
                  class="input"
                  value={location()}
                  placeholder="~/Koral"
                  onInput={(e) => setLocation(e.currentTarget.value)}
                />
                <button type="button" class="btn btn-ghost" onClick={browseLocation}>
                  Browse…
                </button>
              </span>
            </label>

            <Show when={gitUrl().trim() && location()}>
              <p class="field-hint">
                Clones into <code>{joinPath(location(), gitRepoName(gitUrl()))}</code>
              </p>
            </Show>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setImporting(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canImport()}>
                {busy() ? "Importing…" : "Import"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={showSettings() && prefs.s}>
        <div class="modal-scrim" onClick={() => setShowSettings(false)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={savePrefs}>
            <h2 class="modal-title">Settings</h2>
            <p class="field-hint">
              Defaults for new projects and for opening existing ones. Stored on this machine only —
              nothing here travels with a project.
            </p>

            <label class="field">
              <span class="field-label">Default project location</span>
              <span class="field-row">
                <input
                  class="input"
                  value={prefs.s!.projectLocation}
                  placeholder={defaults()?.projectLocation}
                  onInput={(e) => setPrefs("s", "projectLocation", e.currentTarget.value)}
                />
                <button type="button" class="btn btn-ghost" onClick={browseDefaultLocation}>
                  Browse…
                </button>
              </span>
            </label>

            <label class="field">
              <span class="field-label">Open projects with</span>
              <Show
                when={(ides()?.length ?? 0) > 0}
                fallback={
                  <p class="field-hint field-bad">
                    No IDE found on this machine. Install VS Code or CLion and reopen the Hub.
                  </p>
                }
              >
                <select
                  class="input"
                  value={prefs.s!.defaultIde}
                  onChange={(e) => setPrefs("s", "defaultIde", e.currentTarget.value)}
                >
                  {/* Empty = follow whatever is installed, rather than pinning a choice. */}
                  <option value="">Auto ({defaultIde()?.name ?? "none"})</option>
                  <For each={ides()}>
                    {(ide) => <option value={ide.id}>{ide.name}</option>}
                  </For>
                </select>
              </Show>
            </label>

            <label class="field">
              <span class="field-label">Framework version for new projects</span>
              <select
                class="input"
                value={prefs.s!.defaultFrameworkVersion}
                onChange={(e) => setPrefs("s", "defaultFrameworkVersion", e.currentTarget.value)}
              >
                <option value="">Auto (newest installed — {defaults()?.frameworkVersion})</option>
                <For each={frameworkChoices()}>
                  {(v) => (
                    <option value={v.version}>
                      koral {v.version}
                      {v.installed ? "" : " (not installed — will download)"}
                    </option>
                  )}
                </For>
              </select>
            </label>
            <p class="field-hint">
              Existing projects are unaffected — each one records its own version in{" "}
              <code>koral.json</code>.
            </p>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setShowSettings(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={busy()}>
                {busy() ? "Saving…" : "Save"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={removing()}>
        <div class="modal-scrim" onClick={() => setRemoving(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={confirmRemove}>
            <h2 class="modal-title">Remove {removing()!.name}?</h2>
            <p class="field-hint">
              <code>{removing()!.path}</code>
            </p>

            <label class="toggle danger-toggle">
              <input
                type="checkbox"
                checked={deleteFiles()}
                onChange={(e) => setDeleteFiles(e.currentTarget.checked)}
              />
              <span>Also delete the project folder from disk</span>
            </label>

            <p class="field-hint" classList={{ "field-bad": deleteFiles() }}>
              {deleteFiles()
                ? "The folder and everything in it — sources, assets, shaders — is deleted permanently. This cannot be undone."
                : "The project is only removed from this list. Nothing on disk is touched, and you can add it back later."}
            </p>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setRemoving(null)}>
                Cancel
              </button>
              <button
                type="submit"
                class="btn"
                classList={{ "btn-primary": !deleteFiles(), "btn-destructive": deleteFiles() }}
                disabled={busy()}
              >
                {busy() ? "Removing…" : deleteFiles() ? "Delete permanently" : "Remove from list"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={settingsPath() && draft.cfg}>
        <div class="modal-scrim" onClick={closeSettings}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={saveSettings}>
            <h2 class="modal-title">{draft.cfg!.name} — Run Settings</h2>
            <p class="field-hint">
              Saved to <code>koral.json</code> and applied on the next build — in the Hub, VS Code
              and CLion alike.
            </p>

            <div class="field-grid">
              {/* A Job runs headless — it has no window, so width/height/flags do not apply and
                  the Hub does not pass them. Only the graphics API is still meaningful. */}
              <Show when={draft.cfg!.kind === "Scene"}>
                <label class="field">
                  <span class="field-label">Width</span>
                  <input
                    class="input"
                    type="number"
                    min="1"
                    value={draft.cfg!.rendering.window.width}
                    onInput={(e) =>
                      setDraft("cfg", "rendering", "window", "width", +e.currentTarget.value)
                    }
                  />
                </label>
                <label class="field">
                  <span class="field-label">Height</span>
                  <input
                    class="input"
                    type="number"
                    min="1"
                    value={draft.cfg!.rendering.window.height}
                    onInput={(e) =>
                      setDraft("cfg", "rendering", "window", "height", +e.currentTarget.value)
                    }
                  />
                </label>
              </Show>
              <label class="field">
                <span class="field-label">Graphics API</span>
                <select
                  class="input"
                  value={draft.cfg!.rendering.api}
                  onChange={(e) =>
                    setDraft("cfg", "rendering", "api", e.currentTarget.value as "Vulkan" | "OpenGL")
                  }
                >
                  <option value="Vulkan">Vulkan</option>
                  <option value="OpenGL">OpenGL</option>
                </select>
              </label>
            </div>

            <Show
              when={draft.cfg!.kind === "Scene"}
              fallback={
                <p class="field-hint">
                  This is a <strong>Job</strong> — it runs headless on a device-only context, so
                  there are no window settings.
                </p>
              }
            >
              <span class="field-label">Window</span>
              <div class="toggle-row">
                <For
                  each={
                    [
                      ["resizable", "Resizable"],
                      ["vsync", "VSync"],
                      ["fullscreen", "Fullscreen"],
                      ["borderless", "Borderless"],
                      ["transparent", "Transparent"],
                    ] as const
                  }
                >
                  {([key, label]) => (
                    <label class="toggle">
                      <input
                        type="checkbox"
                        checked={draft.cfg!.rendering.window[key]}
                        onChange={(e) =>
                          setDraft("cfg", "rendering", "window", key, e.currentTarget.checked)
                        }
                      />
                      <span>{label}</span>
                    </label>
                  )}
                </For>
              </div>
            </Show>

            <For
              each={
                [
                  ["assetDirectories", "Asset folders", "assets"],
                  ["shaderDirectories", "Shader folders", "shaders"],
                ] as const
              }
            >
              {([key, label, placeholder]) => (
                <div class="field">
                  <span class="field-label">{label}</span>
                  <For each={draft.cfg!.paths[key]}>
                    {(dir, i) => (
                      <div class="dir-row">
                        <input
                          class="input"
                          value={dir}
                          placeholder={placeholder}
                          onInput={(e) =>
                            setDraft("cfg", "paths", key, i(), e.currentTarget.value)
                          }
                        />
                        {/* Order is the search order, so moving an entry up is a real setting. */}
                        <button
                          type="button"
                          class="btn btn-ghost btn-icon btn-move"
                          title="Search this one earlier"
                          disabled={i() === 0}
                          onClick={() =>
                            setDraft("cfg", "paths", key, (dirs) => {
                              const next = [...dirs];
                              [next[i() - 1], next[i()]] = [next[i()], next[i() - 1]];
                              return next;
                            })
                          }
                        >
                          ↑
                        </button>
                        <button
                          type="button"
                          class="btn btn-ghost btn-icon"
                          title="Remove"
                          onClick={() =>
                            setDraft("cfg", "paths", key, (dirs) =>
                              dirs.filter((_, n) => n !== i()),
                            )
                          }
                        >
                          ✕
                        </button>
                      </div>
                    )}
                  </For>
                  <button
                    type="button"
                    class="btn btn-ghost btn-small"
                    onClick={() => setDraft("cfg", "paths", key, (dirs) => [...dirs, ""])}
                  >
                    + Add folder
                  </button>
                </div>
              )}
            </For>
            <p class="field-hint">
              Relative to the project root, searched in order. The runtime resolves relative texture,
              model and shader paths against these — a scene can just ask for{" "}
              <code>textures/wood.png</code>. The engine's own content is searched last, so a project
              can shadow a built-in asset by name without losing the rest.
            </p>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={closeSettings}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={busy()}>
                {busy() ? "Saving…" : "Save"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={log()}>
        <section class="console">
          <div class="console-head">
            <span>{running() ? "Building…" : "Build output"}</span>
            <button class="btn btn-icon" title="Clear" onClick={() => setLog("")}>
              ✕
            </button>
          </div>
          <pre class="console-body">{log()}</pre>
        </section>
      </Show>
    </div>
  );
}
