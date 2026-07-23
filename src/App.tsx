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

// Mirrors `Lab` — one downloadable lab in a collection, hosted in its own git repo.
type Lab = { name: string; description: string; url: string };

// Mirrors `CollectionManifest` — the document an instructor publishes to describe a course's labs.
type CollectionManifest = {
  schemaVersion: number;
  title: string;
  description: string;
  labs: Lab[];
};

// Mirrors `CollectionView` — a subscribed collection with either its fetched manifest or the error
// fetching it produced. The error is per-collection so one bad URL never blanks the rest.
type CollectionView = {
  url: string;
  manifest: CollectionManifest | null;
  error: string | null;
};

// Mirrors `AuthoredCollection` — a collection the user is building locally, listed under
// "My Collections". `path` is machine-local; the rest is read from its koral-collection.json.
type AuthoredCollection = {
  path: string;
  title: string;
  description: string;
  labCount: number;
  labs: Lab[];
  git: GitInfo | null;
};

// A normalized target for the publish dialog — either a collection or a project, so one dialog
// serves both "publish/update this collection" and "save this project to my git".
type PublishTarget = {
  kind: "collection" | "project";
  path: string;
  title: string;
  remote: string | null;
  // Whether a signed-in account owns `remote` — decides "push updates" vs "create a new repo".
  ownsRemote: boolean;
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
  frameworkVersion: string;
  rendering: {
    api: "Vulkan" | "OpenGL";
    // Linux windowing system the Scene opens on; ignored on other platforms. Editable below.
    platform: "auto" | "x11" | "wayland";
    window: {
      width: number;
      height: number;
      resizable: boolean;
      fullscreen: boolean;
      borderless: boolean;
      transparent: boolean;
      vsync: boolean;
      // Where ImGui saves its layout. Not edited in the UI — carried through so a save never drops
      // it — a user relocates it by hand in koral.json.
      imguiIni?: string;
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
  // Linux only: "wayland", "x11", or "" for the session default.
  displayBackend: string;
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

// Mirrors `Provider` (serde lowercase) and the account/sign-in DTOs from the auth module.
type Provider = "github" | "gitlab";
type AccountView = { provider: Provider; host: string; username: string };
// What the device flow shows the user: a code, and where to enter it.
type DeviceLogin = {
  userCode: string;
  verificationUri: string;
  verificationUriComplete: string | null;
};
type DeviceLoginFinished = { success: boolean; account: AccountView | null; error: string | null };
// Result of publishing an authored collection.
type PublishResult = { url: string; created: boolean };

type Finished = { success: boolean; error: string | null };
type InstallProgress = { version: string; downloaded: number; total: number };
type InstallFinished = { version: string; success: boolean; error: string | null };

// The main content is split across these tabs so no single screen carries every list at once.
type Tab = "projects" | "collections" | "framework";
const TABS: readonly (readonly [Tab, string])[] = [
  ["projects", "Projects"],
  ["collections", "Collections"],
  ["framework", "Framework"],
] as const;

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

// --- Terminal (ANSI) rendering for the console ---
// The 8 normal + 8 bright colours, tuned to read on the dark console background.
const ANSI_FG = [
  "#5c5650", "#e05555", "#3fb950", "#d0a215", "#4a9eff", "#c76bd6", "#39c5cf", "#c9c5bf",
];
const ANSI_BRIGHT = [
  "#7a746c", "#ff7b72", "#56d364", "#e3b341", "#79b8ff", "#d98ce6", "#56d4dd", "#f0ede9",
];

type AnsiSeg = { text: string; color?: string; bg?: string; bold?: boolean };

// One xterm-256 palette index to a CSS colour: the 16 base colours, the 6×6×6 cube, then greys.
function ansi256(idx: number): string {
  if (idx < 8) return ANSI_FG[idx];
  if (idx < 16) return ANSI_BRIGHT[idx - 8];
  if (idx < 232) {
    const n = idx - 16;
    const lv = (v: number) => (v === 0 ? 0 : 55 + v * 40);
    return `rgb(${lv(Math.floor(n / 36))}, ${lv(Math.floor((n % 36) / 6))}, ${lv(n % 6)})`;
  }
  const v = 8 + (idx - 232) * 10;
  return `rgb(${v}, ${v}, ${v})`;
}

// Parse a string carrying ANSI escapes into styled segments. SGR (colour/weight) sequences set the
// style; every other escape — cursor moves, line erases (ESC[K), window-title OSC — is swallowed so
// it never shows as stray letters. Supports 16-colour, 256-colour and truecolour foreground/back.
function parseAnsi(input: string): AnsiSeg[] {
  // Drop OSC sequences (ESC ] … BEL or ESC \) wholesale — they carry no styling we render.
  // eslint-disable-next-line no-control-regex
  input = input.replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "");

  const segs: AnsiSeg[] = [];
  let color: string | undefined;
  let bg: string | undefined;
  let bold = false;
  const push = (text: string) => {
    if (text) segs.push({ text, color, bg, bold });
  };
  // Any CSI sequence: ESC [ params finalByte. Only a final 'm' is SGR; the rest are dropped.
  // eslint-disable-next-line no-control-regex
  const re = /\x1b\[([0-9;]*)([A-Za-z])/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(input))) {
    push(input.slice(last, m.index));
    last = re.lastIndex;
    if (m[2] !== "m") continue; // non-SGR CSI (cursor move, erase, …): swallow, don't render
    const codes = m[1] === "" ? [0] : m[1].split(";").map((s) => Number(s) || 0);
    for (let i = 0; i < codes.length; i++) {
      const c = codes[i];
      if (c === 0) {
        color = undefined;
        bg = undefined;
        bold = false;
      } else if (c === 1) bold = true;
      else if (c === 22) bold = false;
      else if (c >= 30 && c <= 37) color = ANSI_FG[c - 30];
      else if (c === 39) color = undefined;
      else if (c >= 90 && c <= 97) color = ANSI_BRIGHT[c - 90];
      else if (c >= 40 && c <= 47) bg = ANSI_FG[c - 40];
      else if (c === 49) bg = undefined;
      else if (c >= 100 && c <= 107) bg = ANSI_BRIGHT[c - 100];
      else if (c === 38 || c === 48) {
        // Extended colour: 38;5;n / 48;5;n (256) or 38;2;r;g;b / 48;2;r;g;b (truecolour).
        const mode = codes[i + 1];
        let col: string | undefined;
        if (mode === 5) {
          col = ansi256(codes[i + 2] ?? 0);
          i += 2;
        } else if (mode === 2) {
          col = `rgb(${codes[i + 2] ?? 0}, ${codes[i + 3] ?? 0}, ${codes[i + 4] ?? 0})`;
          i += 4;
        }
        if (col) {
          if (c === 38) color = col;
          else bg = col;
        }
      }
    }
  }
  push(input.slice(last));
  return segs;
}

// Collapse carriage-return overwrites: a terminal returns to column 0 on \r and overwrites, so for
// each line we keep only what follows its last \r — which turns a \r-updated progress line into its
// final state instead of a wall of intermediate frames.
function collapseCr(text: string): string {
  return text
    .split("\n")
    .map((line) => {
      // A trailing CR is just a CRLF line ending (a PTY emits these) — not an overwrite.
      if (line.endsWith("\r")) line = line.slice(0, -1);
      // A remaining interior CR means the terminal returned to column 0 and overwrote, so keep only
      // what follows the last one — the line's final rendered state (e.g. a progress bar's end).
      const i = line.lastIndexOf("\r");
      return i >= 0 ? line.slice(i + 1) : line;
    })
    .join("\n");
}

// Render console text with its ANSI colours. Kept lean: one <span> per styled run.
function AnsiLog(props: { text: string }) {
  const segs = () => parseAnsi(collapseCr(props.text));
  return (
    <For each={segs()}>
      {(s) => (
        <span
          style={{
            color: s.color,
            "background-color": s.bg,
            "font-weight": s.bold ? "700" : undefined,
          }}
        >
          {s.text}
        </span>
      )}
    </For>
  );
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

  // Build output (compile progress/diagnostics) and run output (the launched app's stdout/stderr)
  // are kept apart so the console can show them on separate tabs. Each is capped so a chatty app
  // can't grow the log unbounded.
  const [buildLog, setBuildLog] = createSignal<string>("");
  const [runLog, setRunLog] = createSignal<string>("");
  const [consoleTab, setConsoleTab] = createSignal<"build" | "output">("build");
  const [running, setRunning] = createSignal(false);
  // Whether the current/last job was a run (▶) rather than a build-only, so we know to flip to the
  // Output tab once its build succeeds.
  const [lastJobRun, setLastJobRun] = createSignal(false);

  const LOG_CAP = 200_000;
  const appendCapped = (setter: (fn: (prev: string) => string) => void, chunk: string) =>
    setter((prev) => {
      const next = prev + chunk;
      return next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
    });

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

  // Lab collections: catalogs of downloadable starter projects. Fetched off the network on every
  // open (a course can revise its labs after subscribing), so the resource can be slow — but a
  // single unreachable collection surfaces as that row's error, not a rejected resource.
  const [collections, { refetch: refetchCollections }] = createResource<CollectionView[]>(() =>
    invoke("list_collections"),
  );
  // Add-collection dialog (subscribe to a remote collection to browse).
  const [showAddCollection, setAddingCollection] = createSignal(false);
  const [collectionUrl, setCollectionUrl] = createSignal("");
  const canAddCollection = () => !!collectionUrl().trim() && !busy();
  // The lab currently downloading (its git URL), so only its button shows the pending state.
  const [downloadingLab, setDownloadingLab] = createSignal<string | null>(null);

  // Collapsed collection cards, keyed by URL (imported) or path (authored). Default expanded;
  // adding a key hides that card's body so a long list of collections stays scannable.
  const [collapsed, setCollapsed] = createSignal<Set<string>>(new Set());
  const isCollapsed = (key: string) => collapsed().has(key);
  function toggleCollapsed(key: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(key) ? next.delete(key) : next.add(key);
      return next;
    });
  }

  // Which tab is showing. Projects first — it's what most sessions open for.
  const [tab, setTab] = createSignal<Tab>("projects");

  // Collections the user is authoring locally ("My Collections"). Local repos, always readable.
  const [authored, { refetch: refetchAuthored }] = createResource<AuthoredCollection[]>(() =>
    invoke("list_authored_collections"),
  );
  // Create-collection dialog. Shares the create dialog's `location` (same default folder).
  const [showCreateCollection, setCreatingCollection] = createSignal(false);
  const [collectionName, setCollectionName] = createSignal("");
  const [collectionDescription, setCollectionDescription] = createSignal("");
  const canCreateCollection = () => !!collectionName().trim() && !!location() && !busy();
  // Add-project dialog: the authored collection being added to. Two modes — pick one of your own
  // projects (default), or add any repo by git URL. A local-only project is published first, so the
  // picker mode also carries the publish fields (account/name/private).
  const [addProjectTo, setAddProjectTo] = createSignal<AuthoredCollection | null>(null);
  const [addMode, setAddMode] = createSignal<"project" | "url">("project");
  const [addProjectPath, setAddProjectPath] = createSignal("");
  const [addProjectHost, setAddProjectHost] = createSignal("");
  const [addProjectRepoName, setAddProjectRepoName] = createSignal("");
  const [addProjectPrivate, setAddProjectPrivate] = createSignal(false);
  // Shared "add by URL" fields (URL mode).
  const [labUrl, setLabUrl] = createSignal("");
  const [labName, setLabName] = createSignal("");
  // Description shown in the collection — used by both modes.
  const [labDescription, setLabDescription] = createSignal("");
  // The project currently selected in the picker, and whether it still needs publishing.
  const selectedProject = () => projects()?.find((p) => p.path === addProjectPath()) ?? null;
  const selectedNeedsPublish = () => {
    const p = selectedProject();
    return !!p && !p.git?.remote;
  };
  const canAddProject = () => {
    if (busy()) return false;
    if (addMode() === "url") return !!labUrl().trim();
    if (!selectedProject()) return false;
    if (selectedNeedsPublish()) return !!addProjectHost() && !!addProjectRepoName().trim();
    return true;
  };
  // Remove-collection confirmation, with the same explicit delete-files opt-in as removing a project.
  const [removingCollection, setRemovingCollection] = createSignal<AuthoredCollection | null>(null);
  const [deleteCollectionFiles, setDeleteCollectionFiles] = createSignal(false);

  // Signed-in GitHub/GitLab accounts (local, always readable — tokens never reach the UI).
  const [accounts, { refetch: refetchAccounts }] = createResource<AccountView[]>(() =>
    invoke("list_accounts"),
  );
  // GitLab host for sign-in (supports self-hosted); GitHub is always github.com.
  const [gitlabHost] = createSignal("gitlab.com");
  // In-progress device login: the code to show while we wait for the browser authorization, plus
  // which provider it's for. Null when no sign-in is pending.
  const [deviceLogin, setDeviceLogin] = createSignal<(DeviceLogin & { provider: Provider }) | null>(
    null,
  );
  // Publish dialog: a collection or a project being saved to git, plus its form fields and result.
  const [publishTarget, setPublishTarget] = createSignal<PublishTarget | null>(null);
  const [publishHost, setPublishHost] = createSignal("");
  const [publishRepoName, setPublishRepoName] = createSignal("");
  const [publishPrivate, setPublishPrivate] = createSignal(false);
  const [publishResult, setPublishResult] = createSignal<PublishResult | null>(null);
  const canPublish = () => !!publishHost() && !!publishRepoName().trim() && !busy();
  // Re-publish (just push, no account/name) when the target already has a remote the user owns.
  const publishRepublish = () => {
    const t = publishTarget();
    return !!t?.remote && t.ownsRemote;
  };

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

  // The Wayland/X11 choice is Linux-only, so its setting only appears there. The webview's user
  // agent is the simplest reliable signal (WebKitGTK reports "Linux"; WebView2 "Windows"; WKWebView
  // "Macintosh").
  const isLinux = /linux/i.test(navigator.userAgent);

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
  // Path of the project whose "more actions" (⋮) menu is open — only one at a time.
  const [menuFor, setMenuFor] = createSignal<string | null>(null);

  // The framework versions offered in a project's settings: the usual choices, plus the project's
  // own pinned version if it isn't among them (offline, or an old release GitHub no longer lists),
  // so the dropdown always shows what the project is actually on.
  const projectFwChoices = () => {
    const list = frameworkChoices();
    const cur = draft.cfg?.frameworkVersion;
    if (cur && !list.some((v) => v.version === cur)) {
      return [{ version: cur, installed: (installed() ?? []).some((f) => f.version === cur) }, ...list];
    }
    return list;
  };

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
      await listen<string>("build-output", (e) => appendCapped(setBuildLog, e.payload)),
    );
    unlisten.push(
      await listen<string>("run-output", (e) => appendCapped(setRunLog, e.payload)),
    );
    unlisten.push(
      await listen<Finished>("build-finished", (e) => {
        setRunning(false);
        appendCapped(
          setBuildLog,
          e.payload.success ? "\n✓ done\n" : `\n✗ ${e.payload.error ?? "failed"}\n`,
        );
        // A successful run has now launched — show its Output tab. A failed one (or a build-only)
        // stays on Build so the errors are in view.
        if (e.payload.success && lastJobRun()) setConsoleTab("output");
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

    unlisten.push(
      await listen<DeviceLoginFinished>("device-login-finished", (e) => {
        // Only surface a failure if the dialog is still open — if the user cancelled, a later
        // timeout from the abandoned attempt shouldn't pop an error.
        const wasWaiting = !!deviceLogin();
        setDeviceLogin(null);
        if (!e.payload.success && wasWaiting) setError(e.payload.error ?? "sign-in failed");
        refetchAccounts();
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

  // The folder new work lands in, seeded from Settings the first time it's needed. Shared by the
  // create/import dialogs (where it's editable) and lab downloads (which use it silently).
  async function ensureLocation(): Promise<string> {
    let loc = location();
    if (!loc) {
      loc = await invoke<string>("default_project_location");
      setLocation(loc);
    }
    return loc;
  }

  function openAddCollection() {
    setError(null);
    setCollectionUrl("");
    setAddingCollection(true);
  }

  async function submitAddCollection(e: Event) {
    e.preventDefault();
    const url = collectionUrl().trim();
    if (!url) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("add_collection", { url });
      await refetchCollections();
      setAddingCollection(false);
      setCollectionUrl("");
    } catch (e) {
      // Stay in the dialog so the URL can be fixed — the usual failure is a link that doesn't
      // resolve to a collection manifest.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function removeCollection(url: string) {
    setError(null);
    try {
      await invoke("remove_collection", { url });
      await refetchCollections();
    } catch (e) {
      setError(String(e));
    }
  }

  // Download a lab into the default project folder as a fresh project, then surface it in Recent
  // Projects. No dialog — a lab is meant to be one click from the collection into your workspace.
  async function downloadLab(url: string) {
    setError(null);
    setDownloadingLab(url);
    try {
      const loc = await ensureLocation();
      await invoke("download_lab", { req: { url, location: loc } });
      await refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setDownloadingLab(null);
    }
  }

  // --- Authoring a collection ---

  async function openCreateCollection() {
    setError(null);
    setCollectionName("");
    setCollectionDescription("");
    if (!location()) {
      try {
        setLocation(await invoke<string>("default_project_location"));
      } catch (e) {
        setError(String(e));
      }
    }
    setCreatingCollection(true);
  }

  async function submitCreateCollection(e: Event) {
    e.preventDefault();
    const name = collectionName().trim();
    if (!name || !location()) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("create_collection", {
        req: { location: location(), name, description: collectionDescription() },
      });
      await refetchAuthored();
      setCreatingCollection(false);
      setCollectionName("");
      setCollectionDescription("");
    } catch (e) {
      // Stay in the dialog so the name/location can be corrected — usually "already exists".
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function openAddProject(c: AuthoredCollection) {
    setError(null);
    setAddMode("project");
    setAddProjectPath("");
    setAddProjectHost(accounts()?.[0]?.host ?? "");
    setAddProjectRepoName("");
    setAddProjectPrivate(false);
    setLabUrl("");
    setLabName("");
    setLabDescription("");
    setAddProjectTo(c);
  }

  // Selecting a project in the picker defaults its publish repo name to the project's own name.
  function pickProject(p: RecentProject) {
    setAddProjectPath(p.path);
    setAddProjectRepoName(p.name);
  }

  async function submitAddProject(e: Event) {
    e.preventDefault();
    const c = addProjectTo();
    if (!c || !canAddProject()) return;

    setBusy(true);
    setError(null);
    try {
      if (addMode() === "url") {
        await invoke("add_lab_to_collection", {
          req: { path: c.path, url: labUrl().trim(), name: labName(), description: labDescription() },
        });
      } else {
        const p = selectedProject();
        if (!p) return;
        await invoke("add_project_to_collection", {
          req: {
            path: c.path,
            projectPath: p.path,
            description: labDescription(),
            // A project with a remote is added straight from it; only a local-only one is published.
            host: p.git?.remote ? "" : addProjectHost(),
            repoName: addProjectRepoName().trim(),
            private: addProjectPrivate(),
          },
        });
      }
      await refetchAuthored();
      // Auto-publishing gives the project a remote, so refresh the project cards too.
      await refetch();
      setAddProjectTo(null);
    } catch (e) {
      // Stay in the dialog — the usual failures are not signed in, a private repo, or already added.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Path of the collection whose member-project list is doing a remove/reorder, so its controls show
  // a pending state and don't fire twice mid-commit.
  const [collectionBusy, setCollectionBusy] = createSignal<string | null>(null);

  async function removeLab(c: AuthoredCollection, url: string) {
    if (collectionBusy()) return;
    setCollectionBusy(c.path);
    setError(null);
    try {
      await invoke("remove_lab_from_collection", { path: c.path, url });
      await refetchAuthored();
    } catch (e) {
      setError(String(e));
    } finally {
      setCollectionBusy(null);
    }
  }

  async function reorderLab(c: AuthoredCollection, url: string, up: boolean) {
    if (collectionBusy()) return;
    setCollectionBusy(c.path);
    setError(null);
    try {
      await invoke("reorder_lab_in_collection", { path: c.path, url, up });
      await refetchAuthored();
    } catch (e) {
      setError(String(e));
    } finally {
      setCollectionBusy(null);
    }
  }

  function askRemoveCollection(c: AuthoredCollection) {
    setError(null);
    setDeleteCollectionFiles(false);
    setRemovingCollection(c);
  }

  async function confirmRemoveCollection(e: Event) {
    e.preventDefault();
    const c = removingCollection();
    if (!c) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("remove_authored_collection", {
        path: c.path,
        deleteFiles: deleteCollectionFiles(),
      });
      setRemovingCollection(null);
      await refetchAuthored();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // --- Accounts (sign-in) ---

  // Device-flow sign-in: the backend returns a short code to show, the user approves it in their
  // browser, and it emits `device-login-finished`. Secretless — the model for a distributed,
  // offline app (only a public client ID is shipped).
  async function signIn(provider: Provider) {
    setError(null);
    try {
      const host = provider === "gitlab" ? gitlabHost().trim() || "gitlab.com" : undefined;
      const login = await invoke<DeviceLogin>("device_login_start", { provider, host });
      setDeviceLogin({ provider, ...login });
    } catch (e) {
      setError(String(e));
    }
  }

  async function signOut(account: AccountView) {
    setError(null);
    try {
      await invoke("sign_out", { provider: account.provider, host: account.host });
      await refetchAccounts();
    } catch (e) {
      setError(String(e));
    }
  }

  // Open the verification page in the user's real browser (via the backend, which is reliable from a
  // webview). The code + link are also shown as selectable text as a fallback.
  function openVerification() {
    const login = deviceLogin();
    if (login) {
      invoke("open_url", { url: login.verificationUriComplete ?? login.verificationUri }).catch(
        (e) => setError(String(e)),
      );
    }
  }

  // --- Publishing (a collection, or a project to your git) ---

  // Owner ("namespace") and host of a git URL, in HTTPS or SSH form, for the ownership check below.
  function remoteOwner(url: string): { host: string; owner: string } | null {
    let rest = url.trim();
    const scheme = rest.indexOf("://");
    if (scheme >= 0) rest = rest.slice(scheme + 3);
    const at = rest.lastIndexOf("@");
    if (at >= 0) rest = rest.slice(at + 1);
    const m = rest.match(/^([^/:]+)[/:]+(.+)$/);
    if (!m) return null;
    const owner = m[2].split("/")[0];
    if (!m[1] || !owner) return null;
    return { host: m[1], owner };
  }

  // A compact "host · owner/repo" label for a git URL, shown so a project in a collection reveals
  // where it actually lives. Falls back to the raw URL when it doesn't parse.
  function repoLabel(url: string): string {
    const info = remoteOwner(url);
    if (!info) return url;
    let rest = url.trim();
    const scheme = rest.indexOf("://");
    if (scheme >= 0) rest = rest.slice(scheme + 3);
    const at = rest.lastIndexOf("@");
    if (at >= 0) rest = rest.slice(at + 1);
    const m = rest.match(/^[^/:]+[/:]+(.+)$/);
    const repoPath = (m ? m[1] : "").replace(/\.git$/, "").replace(/\/$/, "");
    return repoPath ? `${info.host} · ${repoPath}` : info.host;
  }

  // Whether a project from this lab's repo is already on disk — matched by "host · owner/repo" so
  // https and ssh forms of the same repo still line up. Only true when we can prove it, so an owned
  // clone that kept its origin shows "Downloaded"; a stripped copy simply won't (no false positives).
  function labDownloaded(url: string): boolean {
    const target = repoLabel(url);
    return (projects() ?? []).some((p) => !!p.git?.remote && repoLabel(p.git.remote) === target);
  }

  // Whether a signed-in account owns `remote` — mirrors the backend's `auth::signed_in_owns`, so the
  // UI can label an owned repo "Update" (push) versus "Save to Git" (fork into a new repo).
  function ownRemote(remote: string | null | undefined): boolean {
    if (!remote) return false;
    const info = remoteOwner(remote);
    if (!info) return false;
    return (accounts() ?? []).some(
      (a) =>
        a.host.toLowerCase() === info.host.toLowerCase() &&
        a.username.toLowerCase() === info.owner.toLowerCase(),
    );
  }

  const folderName = (path: string) => path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() ?? "";

  function openPublish(target: PublishTarget, defaultName: string) {
    setError(null);
    setPublishResult(null);
    setPublishTarget(target);
    setPublishRepoName(defaultName);
    setPublishPrivate(false);
    // Only the create path needs an account; pushing to a remote you own needs none.
    setPublishHost(target.remote && target.ownsRemote ? "" : accounts()?.[0]?.host ?? "");
  }

  function openPublishCollection(c: AuthoredCollection) {
    openPublish(
      {
        kind: "collection",
        path: c.path,
        title: c.title,
        remote: c.git?.remote ?? null,
        ownsRemote: !!c.git?.remote,
      },
      folderName(c.path),
    );
  }

  function openPublishProject(p: RecentProject) {
    openPublish(
      {
        kind: "project",
        path: p.path,
        title: p.name,
        remote: p.git?.remote ?? null,
        ownsRemote: ownRemote(p.git?.remote),
      },
      p.name,
    );
  }

  async function submitPublish(e: Event) {
    e.preventDefault();
    const t = publishTarget();
    if (!t) return;
    // Pushing to a remote you own needs no account choice; creating a new repo does.
    const republish = publishRepublish();
    if (!republish && !canPublish()) return;

    setBusy(true);
    setError(null);
    try {
      const cmd = t.kind === "collection" ? "publish_collection" : "publish_project";
      const result = await invoke<PublishResult>(cmd, {
        req: {
          path: t.path,
          host: republish ? "" : publishHost(),
          repoName: publishRepoName().trim(),
          private: publishPrivate(),
        },
      });
      setPublishResult(result);
      // Refresh both lists — a published project or collection now shows a remote.
      await refetchAuthored();
      await refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // True whenever any modal is open, so the page-level error banner can hide (each dialog shows its
  // own error inline).
  const anyModalOpen = () =>
    showCreate() ||
    showImport() ||
    showAddCollection() ||
    showCreateCollection() ||
    !!addProjectTo() ||
    !!settingsPath() ||
    !!removing() ||
    !!removingCollection() ||
    !!deviceLogin() ||
    !!publishTarget() ||
    showSettings();

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
    setBuildLog("");
    setRunLog("");
    setLastJobRun(true);
    setConsoleTab("build");
    setRunning(true);
    try {
      await invoke("run_project", { path });
    } catch (e) {
      setRunning(false);
      setError(String(e));
    }
  }

  async function buildProject(path: string) {
    setError(null);
    setBuildLog("");
    setRunLog("");
    setLastJobRun(false);
    setConsoleTab("build");
    setRunning(true);
    try {
      await invoke("build_project", { path });
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
        <Show when={error() && !anyModalOpen()}>
          <p class="error">{error()}</p>
        </Show>

        <nav class="tabs">
          <For each={TABS}>
            {([id, label]) => (
              <button
                type="button"
                class="tab"
                classList={{ "tab-active": tab() === id }}
                onClick={() => setTab(id)}
              >
                {label}
              </button>
            )}
          </For>
        </nav>

        <Show when={tab() === "projects"}>
        <h1 class="section-title">Recent Projects</h1>

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
                      class="btn btn-build"
                      title="Build only (compile without running)"
                      disabled={running()}
                      onClick={() => buildProject(p.path)}
                    >
                      <svg
                        class="hammer-icon"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        aria-hidden="true"
                      >
                        <path d="m15 12-8.373 8.373a1 1 0 1 1-3-3L12 9" />
                        <path d="m18 15 4-4" />
                        <path d="m21.5 11.5-1.914-1.914A2 2 0 0 1 19 8.172V7l-2.26-2.26a6 6 0 0 0-4.202-1.756L9 2.96l.92.82A6.18 6.18 0 0 1 12 8.4V10l2 2h1.172a2 2 0 0 1 1.414.586L18.5 14.5" />
                      </svg>
                    </button>
                    <button
                      class="btn btn-play"
                      title="Build &amp; Run"
                      disabled={running()}
                      onClick={() => runProject(p.path)}
                    >
                      ▶
                    </button>
                    <div class="menu-wrap">
                      <button
                        class="btn btn-icon"
                        classList={{ "menu-open": menuFor() === p.path }}
                        title="More actions"
                        onClick={() => setMenuFor(menuFor() === p.path ? null : p.path)}
                      >
                        ⋮
                      </button>
                      <Show when={menuFor() === p.path}>
                        <div class="menu-backdrop" onClick={() => setMenuFor(null)} />
                        <div class="card-menu">
                          <Show when={p.git}>
                            <div class="menu-info">
                              <span class="menu-info-branch">
                                {p.git!.branch ?? "detached"}
                                <Show when={p.git!.dirty}>
                                  <span class="git-dot"> ●</span>
                                </Show>
                              </span>
                              <span class="menu-info-remote">
                                {p.git!.remote ?? "local git repository"}
                              </span>
                            </div>
                          </Show>
                          <button
                            type="button"
                            class="menu-item"
                            onClick={() => {
                              setMenuFor(null);
                              openSettings(p.path);
                            }}
                          >
                            Settings
                          </button>
                          <button
                            type="button"
                            class="menu-item"
                            onClick={() => {
                              setMenuFor(null);
                              openPublishProject(p);
                            }}
                          >
                            {p.git?.remote && ownRemote(p.git.remote) ? "Update on Git" : "Save to Git"}
                          </button>
                          <button
                            type="button"
                            class="menu-item menu-danger"
                            onClick={() => {
                              setMenuFor(null);
                              askRemove(p);
                            }}
                          >
                            Remove project
                          </button>
                        </div>
                      </Show>
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
        </Show>

        <Show when={tab() === "collections"}>
        <div class="section-head">
          <h1 class="section-title">Browse</h1>
          <button class="btn btn-ghost btn-small" disabled={busy()} onClick={openAddCollection}>
            + Add collection
          </button>
        </div>

        <Show
          when={!collections.loading}
          fallback={<p class="muted">Loading collections…</p>}
        >
          <Show
            when={(collections()?.length ?? 0) > 0}
            fallback={
              <div class="empty">
                <p class="muted">No collections yet.</p>
                <p class="muted-sm">Add one with a link your instructor shared.</p>
              </div>
            }
          >
            <div class="collection-list">
              <For each={collections()}>
                {(c) => (
                  <section class="collection-card" classList={{ collapsed: isCollapsed(c.url) }}>
                    <div class="collection-head">
                      <button
                        class="btn btn-icon collapse-toggle"
                        title={isCollapsed(c.url) ? "Expand" : "Collapse"}
                        onClick={() => toggleCollapsed(c.url)}
                      >
                        {isCollapsed(c.url) ? "▸" : "▾"}
                      </button>
                      <span class="collection-meta" onClick={() => toggleCollapsed(c.url)}>
                        <span class="collection-title">{c.manifest?.title ?? c.url}</span>
                        <Show when={c.manifest?.description}>
                          <span class="collection-sub">{c.manifest!.description}</span>
                        </Show>
                        <span class="collection-source" title={c.url}>{repoLabel(c.url)}</span>
                      </span>
                      <Show when={c.manifest}>
                        <span class="collection-count">
                          {c.manifest!.labs.length}{" "}
                          {c.manifest!.labs.length === 1 ? "project" : "projects"}
                        </span>
                      </Show>
                      <button
                        class="btn btn-icon btn-danger"
                        title="Remove this collection (downloaded projects are kept)"
                        onClick={() => removeCollection(c.url)}
                      >
                        ✕
                      </button>
                    </div>

                    <Show when={!isCollapsed(c.url)}>
                      <Show when={c.error}>
                        <p class="error">Couldn't load this collection: {c.error}</p>
                      </Show>

                      <Show when={c.manifest}>
                        <Show
                          when={c.manifest!.labs.length > 0}
                          fallback={<p class="muted-sm">This collection has no projects yet.</p>}
                        >
                          <ul class="lab-list">
                            <For each={c.manifest!.labs}>
                              {(lab, i) => (
                                <li class="lab-row">
                                  <span class="lab-index">{i() + 1}</span>
                                  <span class="lab-meta">
                                    <span class="lab-name">
                                      {lab.name}
                                      <Show when={labDownloaded(lab.url)}>
                                        <span class="lab-badge">Downloaded</span>
                                      </Show>
                                    </span>
                                    <Show when={lab.description}>
                                      <span class="lab-desc">{lab.description}</span>
                                    </Show>
                                    <span class="lab-repo" title={lab.url}>{repoLabel(lab.url)}</span>
                                  </span>
                                  <button
                                    class="btn btn-ghost btn-small"
                                    disabled={downloadingLab() !== null}
                                    onClick={() => downloadLab(lab.url)}
                                  >
                                    {downloadingLab() === lab.url
                                      ? "Downloading…"
                                      : labDownloaded(lab.url)
                                        ? "Download again"
                                        : "Download"}
                                  </button>
                                </li>
                              )}
                            </For>
                          </ul>
                        </Show>
                      </Show>
                    </Show>
                  </section>
                )}
              </For>
            </div>
          </Show>
        </Show>

        <div class="section-head">
          <h1 class="section-title">My Collections</h1>
          <button class="btn btn-ghost btn-small" disabled={busy()} onClick={openCreateCollection}>
            + New collection
          </button>
        </div>
        <p class="field-hint collections-blurb">
          Gather your projects into one repository of git submodules, then push it and share the link
          — students add it under <strong>Browse</strong> and download each as a course lab.
        </p>

        <Show when={!authored.loading} fallback={<p class="muted">Loading…</p>}>
          <Show
            when={(authored()?.length ?? 0) > 0}
            fallback={
              <div class="empty">
                <p class="muted">You aren't building any collections yet.</p>
                <p class="muted-sm">Create one to gather projects for a course.</p>
              </div>
            }
          >
            <ul class="collection-list">
              <For each={authored()}>
                {(c) => (
                  <li class="collection-card" classList={{ collapsed: isCollapsed(c.path) }}>
                    <div class="collection-head">
                      <button
                        class="btn btn-icon collapse-toggle"
                        title={isCollapsed(c.path) ? "Expand" : "Collapse"}
                        onClick={() => toggleCollapsed(c.path)}
                      >
                        {isCollapsed(c.path) ? "▸" : "▾"}
                      </button>
                      <span class="collection-meta" onClick={() => toggleCollapsed(c.path)}>
                        <span class="collection-title">{c.title}</span>
                        <Show when={c.description}>
                          <span class="collection-sub">{c.description}</span>
                        </Show>
                        <span class="collection-source" title={c.path}>{c.path}</span>
                      </span>
                      <span class="collection-count">
                        {c.labCount} {c.labCount === 1 ? "project" : "projects"}
                      </span>
                      <Show when={c.git}>
                        <span
                          class="project-git"
                          title={
                            (c.git!.remote ?? "local git repository") +
                            (c.git!.dirty ? " · uncommitted changes" : "")
                          }
                        >
                          <span class="git-branch-name">{c.git!.branch ?? "detached"}</span>
                          <Show when={c.git!.dirty}>
                            <span class="git-dot">●</span>
                          </Show>
                        </span>
                      </Show>
                      <button
                        class="btn btn-ghost btn-small"
                        disabled={!!addProjectTo()}
                        onClick={() => openAddProject(c)}
                      >
                        + Add project
                      </button>
                      <button
                        class="btn btn-ghost btn-small"
                        title={c.git?.remote ? `Push updates to ${c.git.remote}` : "Publish to GitHub"}
                        onClick={() => openPublishCollection(c)}
                      >
                        {c.git?.remote ? "Publish updates" : "Publish"}
                      </button>
                      <button
                        class="btn btn-icon btn-danger"
                        title="Remove this collection"
                        onClick={() => askRemoveCollection(c)}
                      >
                        ✕
                      </button>
                    </div>

                    {/* The projects in the collection, in order — reorder or remove each. */}
                    <Show when={!isCollapsed(c.path)}>
                    <Show
                      when={c.labs.length > 0}
                      fallback={
                        <p class="collection-empty muted-sm">
                          No projects yet — add one with “+ Add project”.
                        </p>
                      }
                    >
                      <ul class="member-list">
                        <For each={c.labs}>
                          {(lab, i) => (
                            <li class="member-row">
                              <span class="member-index">{i() + 1}</span>
                              <span class="member-meta">
                                <span class="member-name">{lab.name}</span>
                                <Show when={lab.description || lab.url}>
                                  <span class="member-sub">{lab.description || lab.url}</span>
                                </Show>
                              </span>
                              <span class="member-actions">
                                <button
                                  class="btn btn-icon btn-move"
                                  title="Move up"
                                  disabled={i() === 0 || collectionBusy() === c.path}
                                  onClick={() => reorderLab(c, lab.url, true)}
                                >
                                  ↑
                                </button>
                                <button
                                  class="btn btn-icon btn-move"
                                  title="Move down"
                                  disabled={i() === c.labs.length - 1 || collectionBusy() === c.path}
                                  onClick={() => reorderLab(c, lab.url, false)}
                                >
                                  ↓
                                </button>
                                <button
                                  class="btn btn-icon btn-danger"
                                  title="Remove from collection"
                                  disabled={collectionBusy() === c.path}
                                  onClick={() => removeLab(c, lab.url)}
                                >
                                  ✕
                                </button>
                              </span>
                            </li>
                          )}
                        </For>
                      </ul>
                    </Show>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
        </Show>

        <Show when={tab() === "framework"}>
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

      <Show when={showAddCollection()}>
        <div class="modal-scrim" onClick={() => setAddingCollection(false)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitAddCollection}>
            <h2 class="modal-title">Add Collection</h2>
            <p class="field-hint">
              Paste the link your instructor shared — a GitHub repository, or a direct link to its{" "}
              <code>koral-collection.json</code>. The Hub remembers it and refreshes its projects each
              time you open.
            </p>

            <label class="field">
              <span class="field-label">Collection URL</span>
              <input
                class="input"
                value={collectionUrl()}
                placeholder="https://github.com/prof/graphics-labs"
                autofocus
                onInput={(e) => setCollectionUrl(e.currentTarget.value)}
              />
            </label>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setAddingCollection(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canAddCollection()}>
                {busy() ? "Adding…" : "Add"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={showCreateCollection()}>
        <div class="modal-scrim" onClick={() => setCreatingCollection(false)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitCreateCollection}>
            <h2 class="modal-title">New Collection</h2>
            <p class="field-hint">
              Creates a git repository that gathers projects as submodules. Add projects to it
              afterwards, then push it and share its URL for students to browse.
            </p>

            <label class="field">
              <span class="field-label">Name</span>
              <input
                class="input"
                value={collectionName()}
                placeholder="Intro to Graphics — Labs"
                autofocus
                onInput={(e) => setCollectionName(e.currentTarget.value)}
              />
            </label>

            <label class="field">
              <span class="field-label">Description</span>
              <input
                class="input"
                value={collectionDescription()}
                placeholder="Lab collection for CS-4560."
                onInput={(e) => setCollectionDescription(e.currentTarget.value)}
              />
            </label>

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

            <Show when={location() && collectionName().trim()}>
              <p class="field-hint">
                Creates <code>{joinPath(location(), collectionName().trim())}</code>
              </p>
            </Show>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setCreatingCollection(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canCreateCollection()}>
                {busy() ? "Creating…" : "Create"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={addProjectTo()}>
        <div class="modal-scrim" onClick={() => setAddProjectTo(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitAddProject}>
            <h2 class="modal-title">Add project to {addProjectTo()!.title}</h2>

            {/* Pick one of your own projects, or add any repository by URL. */}
            <div class="segmented">
              <button
                type="button"
                classList={{ active: addMode() === "project" }}
                onClick={() => setAddMode("project")}
              >
                Your projects
              </button>
              <button
                type="button"
                classList={{ active: addMode() === "url" }}
                onClick={() => setAddMode("url")}
              >
                Git URL
              </button>
            </div>

            <Show
              when={addMode() === "project"}
              fallback={
                <>
                  <p class="field-hint">
                    Any git repository, added as a submodule. Its folder name comes from the repo.
                  </p>
                  <label class="field">
                    <span class="field-label">Repository URL</span>
                    <input
                      class="input"
                      value={labUrl()}
                      placeholder="https://github.com/course/lab01-triangle.git"
                      onInput={(e) => setLabUrl(e.currentTarget.value)}
                    />
                  </label>
                  <label class="field">
                    <span class="field-label">Display name (optional)</span>
                    <input
                      class="input"
                      value={labName()}
                      placeholder={labUrl().trim() ? gitRepoName(labUrl()) : "Lab 01 — Triangle"}
                      onInput={(e) => setLabName(e.currentTarget.value)}
                    />
                  </label>
                </>
              }
            >
              <p class="field-hint">
                Added as a submodule tracking the project's git repository.
              </p>
              <Show
                when={(projects()?.length ?? 0) > 0}
                fallback={<p class="field-hint field-bad">You don't have any projects yet.</p>}
              >
                <div class="project-picker">
                  <For each={projects()}>
                    {(p) => (
                      <label class="pick-row" classList={{ selected: addProjectPath() === p.path }}>
                        <input
                          type="radio"
                          name="add-project"
                          checked={addProjectPath() === p.path}
                          onChange={() => pickProject(p)}
                        />
                        <span class="pick-meta">
                          <span class="pick-name">{p.name}</span>
                          <span class="pick-sub">{p.git?.remote ?? p.path}</span>
                        </span>
                        <Show when={!p.git?.remote}>
                          <span class="pick-tag">not published</span>
                        </Show>
                      </label>
                    )}
                  </For>
                </div>
              </Show>

              {/* A local-only project has no URL for a submodule, so publish it first. */}
              <Show when={selectedNeedsPublish()}>
                <Show
                  when={(accounts()?.length ?? 0) > 0}
                  fallback={
                    <p class="field-hint field-bad">
                      This project isn't published yet. Sign in to GitHub first (Settings →
                      Accounts).
                    </p>
                  }
                >
                  <p class="field-hint">
                    This project isn't published yet — it'll be pushed to a new repository first.
                  </p>
                  <label class="field">
                    <span class="field-label">Account</span>
                    <select
                      class="input"
                      value={addProjectHost()}
                      onChange={(e) => setAddProjectHost(e.currentTarget.value)}
                    >
                      <For each={accounts()}>
                        {(a) => (
                          <option value={a.host}>
                            {a.username}@{a.host}
                          </option>
                        )}
                      </For>
                    </select>
                  </label>
                  <label class="field">
                    <span class="field-label">Repository name</span>
                    <input
                      class="input"
                      value={addProjectRepoName()}
                      onInput={(e) => setAddProjectRepoName(e.currentTarget.value)}
                    />
                  </label>
                  <label class="toggle">
                    <input
                      type="checkbox"
                      checked={addProjectPrivate()}
                      onChange={(e) => setAddProjectPrivate(e.currentTarget.checked)}
                    />
                    <span>Private repository</span>
                  </label>
                </Show>
              </Show>
            </Show>

            <label class="field">
              <span class="field-label">Description (optional)</span>
              <input
                class="input"
                value={labDescription()}
                placeholder="Draw your first triangle."
                onInput={(e) => setLabDescription(e.currentTarget.value)}
              />
            </label>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setAddProjectTo(null)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canAddProject()}>
                {busy() ? "Adding…" : "Add project"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={removingCollection()}>
        <div class="modal-scrim" onClick={() => setRemovingCollection(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={confirmRemoveCollection}>
            <h2 class="modal-title">Remove {removingCollection()!.title}?</h2>
            <p class="field-hint">
              <code>{removingCollection()!.path}</code>
            </p>

            <label class="toggle danger-toggle">
              <input
                type="checkbox"
                checked={deleteCollectionFiles()}
                onChange={(e) => setDeleteCollectionFiles(e.currentTarget.checked)}
              />
              <span>Also delete the collection folder from disk</span>
            </label>

            <p class="field-hint" classList={{ "field-bad": deleteCollectionFiles() }}>
              {deleteCollectionFiles()
                ? "The folder and everything in it — including the submodule checkouts — is deleted permanently. This cannot be undone."
                : "The collection is only removed from this list. Nothing on disk is touched."}
            </p>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setRemovingCollection(null)}>
                Cancel
              </button>
              <button
                type="submit"
                class="btn"
                classList={{
                  "btn-primary": !deleteCollectionFiles(),
                  "btn-destructive": deleteCollectionFiles(),
                }}
                disabled={busy()}
              >
                {busy() ? "Removing…" : deleteCollectionFiles() ? "Delete permanently" : "Remove from list"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      <Show when={showSettings() && prefs.s}>
        <div class="modal-scrim" onClick={() => setShowSettings(false)}>
          <form class="modal modal-tall" onClick={(e) => e.stopPropagation()} onSubmit={savePrefs}>
            <h2 class="modal-title">Settings</h2>

            <div class="modal-scroll">
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

            <Show when={isLinux}>
              <label class="field">
                <span class="field-label">Display server (Linux)</span>
                <select
                  class="input"
                  value={prefs.s!.displayBackend}
                  onChange={(e) => setPrefs("s", "displayBackend", e.currentTarget.value)}
                >
                  <option value="">Auto (session default)</option>
                  <option value="wayland">Wayland</option>
                  <option value="x11">X11</option>
                </select>
              </label>
              <p class="field-hint">
                Which windowing backend a launched app uses — applied to the app you run, not the Hub.
              </p>
            </Show>

            <hr class="modal-divider" />

            <span class="field-label">Accounts</span>
            <p class="field-hint">
              Sign in to clone private projects and to publish your own projects and collections.
              Stored on this machine only.
            </p>

            <Show when={(accounts()?.length ?? 0) > 0}>
              <ul class="account-list">
                <For each={accounts()}>
                  {(a) => (
                    <li class="account-row">
                      <span class="account-meta">
                        <span class="account-name">
                          {a.username}
                          <span class="account-host">@{a.host}</span>
                        </span>
                        <span class="account-provider">{a.provider}</span>
                      </span>
                      <button
                        type="button"
                        class="btn btn-ghost btn-small"
                        onClick={() => signOut(a)}
                      >
                        Sign out
                      </button>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <div class="account-actions">
              <button type="button" class="btn btn-ghost btn-small" onClick={() => signIn("github")}>
                Sign in to GitHub
              </button>
            </div>
            </div>

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

      <Show when={deviceLogin()}>
        <div class="modal-scrim">
          <div class="modal" onClick={(e) => e.stopPropagation()}>
            <h2 class="modal-title">
              Sign in to {deviceLogin()!.provider === "github" ? "GitHub" : "GitLab"}
            </h2>
            <p class="field-hint">
              Open the page below and enter this code to authorize Koral Hub. This dialog closes
              itself once you're done.
            </p>

            <div class="device-code">{deviceLogin()!.userCode}</div>

            <p class="field-hint">
              At{" "}
              <code class="device-uri">
                {deviceLogin()!.verificationUriComplete ?? deviceLogin()!.verificationUri}
              </code>
            </p>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setDeviceLogin(null)}>
                Cancel
              </button>
              <button type="button" class="btn btn-primary" onClick={openVerification}>
                Open page
              </button>
            </div>
          </div>
        </div>
      </Show>

      <Show when={publishTarget()}>
        <div class="modal-scrim" onClick={() => setPublishTarget(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitPublish}>
            <h2 class="modal-title">
              {publishTarget()!.kind === "collection"
                ? `Publish ${publishTarget()!.title}`
                : publishRepublish()
                  ? `Update ${publishTarget()!.title}`
                  : `Save ${publishTarget()!.title} to your Git`}
            </h2>

            {/* Once it has a remote you own, publishing again just pushes — no account/name to choose. */}
            <Show
              when={!publishResult()}
              fallback={
                <>
                  <p class="field-hint">
                    {publishTarget()!.kind === "collection" ? (
                      <>
                        {publishResult()!.created ? "Published." : "Pushed your latest changes."}{" "}
                        Students add this collection under <strong>Browse</strong> with:
                      </>
                    ) : publishResult()!.created ? (
                      "Saved to your git at:"
                    ) : (
                      "Pushed your latest changes to:"
                    )}
                  </p>
                  <code class="device-uri">{publishResult()!.url}</code>
                  <div class="modal-actions">
                    <button
                      type="button"
                      class="btn btn-primary"
                      onClick={() => setPublishTarget(null)}
                    >
                      Done
                    </button>
                  </div>
                </>
              }
            >
              <Show
                when={!publishRepublish()}
                fallback={
                  <>
                    <p class="field-hint">
                      {publishTarget()!.kind === "collection"
                        ? "Pushes the new commits (projects you've added) to "
                        : "Pushes your changes to "}
                      <code>{publishTarget()!.remote}</code>.
                    </p>
                    <Show when={error()}>
                      <p class="error">{error()}</p>
                    </Show>
                    <div class="modal-actions">
                      <button type="button" class="btn btn-ghost" onClick={() => setPublishTarget(null)}>
                        Cancel
                      </button>
                      <button type="submit" class="btn btn-primary" disabled={busy()}>
                        {busy() ? "Pushing…" : "Push updates"}
                      </button>
                    </div>
                  </>
                }
              >
                <Show when={publishTarget()!.kind === "project" && !!publishTarget()!.remote}>
                  <p class="field-hint">
                    This project's repository is someone else's, so it's copied to a new one under
                    your account.
                  </p>
                </Show>
                <Show
                  when={(accounts()?.length ?? 0) > 0}
                  fallback={
                    <p class="field-hint field-bad">
                      Sign in to GitHub first (Settings → Accounts).
                    </p>
                  }
                >
                  <label class="field">
                    <span class="field-label">Account</span>
                    <select
                      class="input"
                      value={publishHost()}
                      onChange={(e) => setPublishHost(e.currentTarget.value)}
                    >
                      <For each={accounts()}>
                        {(a) => (
                          <option value={a.host}>
                            {a.username}@{a.host}
                          </option>
                        )}
                      </For>
                    </select>
                  </label>

                  <label class="field">
                    <span class="field-label">Repository name</span>
                    <input
                      class="input"
                      value={publishRepoName()}
                      onInput={(e) => setPublishRepoName(e.currentTarget.value)}
                    />
                  </label>

                  <label class="toggle">
                    <input
                      type="checkbox"
                      checked={publishPrivate()}
                      onChange={(e) => setPublishPrivate(e.currentTarget.checked)}
                    />
                    <span>Private repository</span>
                  </label>

                  <Show when={error()}>
                    <p class="error">{error()}</p>
                  </Show>

                  <div class="modal-actions">
                    <button type="button" class="btn btn-ghost" onClick={() => setPublishTarget(null)}>
                      Cancel
                    </button>
                    <button type="submit" class="btn btn-primary" disabled={!canPublish()}>
                      {busy()
                        ? "Saving…"
                        : publishTarget()!.kind === "collection"
                          ? "Publish"
                          : "Save to Git"}
                    </button>
                  </div>
                </Show>
              </Show>
            </Show>
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
            <h2 class="modal-title">{draft.cfg!.name} — Settings</h2>
            <p class="field-hint">
              Saved to <code>koral.json</code> and applied on the next build — in the Hub, VS Code
              and CLion alike.
            </p>

            <label class="field">
              <span class="field-label">Framework version</span>
              <select
                class="input"
                value={draft.cfg!.frameworkVersion}
                onChange={(e) => setDraft("cfg", "frameworkVersion", e.currentTarget.value)}
              >
                <For each={projectFwChoices()}>
                  {(v) => (
                    <option value={v.version}>
                      koral {v.version}
                      {v.installed ? "" : " (not installed — will download)"}
                    </option>
                  )}
                </For>
              </select>
            </label>

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
              {/* A Scene's windowing system on Linux. Ignored on Windows/macOS, so only shown there;
                  a Job has no window. `auto` lets the runtime (GLFW) choose. Note OpenGL always runs
                  on X11/XWayland — a Wayland choice with OpenGL is ignored by the runtime. */}
              <Show when={isLinux && draft.cfg!.kind === "Scene"}>
                <label class="field">
                  <span class="field-label">Windowing (Linux)</span>
                  <select
                    class="input"
                    value={draft.cfg!.rendering.platform ?? "auto"}
                    onChange={(e) =>
                      setDraft(
                        "cfg",
                        "rendering",
                        "platform",
                        e.currentTarget.value as "auto" | "x11" | "wayland",
                      )
                    }
                  >
                    <option value="auto">Auto (GLFW default)</option>
                    <option value="wayland">Wayland</option>
                    <option value="x11">X11</option>
                  </select>
                </label>
              </Show>
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

      <Show when={buildLog() || runLog()}>
        <section class="console">
          <div class="console-head">
            <div class="console-tabs">
              <Show when={buildLog()}>
                <button
                  type="button"
                  class="console-tab"
                  classList={{ active: consoleTab() === "build" }}
                  onClick={() => setConsoleTab("build")}
                >
                  {running() ? "Building…" : "Build"}
                </button>
              </Show>
              <Show when={runLog()}>
                <button
                  type="button"
                  class="console-tab"
                  classList={{ active: consoleTab() === "output" }}
                  onClick={() => setConsoleTab("output")}
                >
                  Output
                </button>
              </Show>
            </div>
            <button
              class="btn btn-icon"
              title="Close this tab"
              onClick={() => {
                // Clear the active tab and hand focus to the other. If that one is empty too, the
                // whole console's `Show` (buildLog || runLog) goes false and the panel disappears —
                // so closing the last tab closes the console instead of leaving an empty shell.
                if (consoleTab() === "build") {
                  setBuildLog("");
                  setConsoleTab("output");
                } else {
                  setRunLog("");
                  setConsoleTab("build");
                }
              }}
            >
              ✕
            </button>
          </div>
          <pre class="console-body">
            <AnsiLog text={consoleTab() === "build" ? buildLog() : runLog()} />
          </pre>
        </section>
      </Show>
    </div>
  );
}
