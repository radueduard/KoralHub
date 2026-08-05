import { createEffect, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import logo from "./assets/logo.png";
import "./App.css";

// Mirrors the `RecentProject` DTO returned by the Rust commands.
// Scene = realtime app with an Update/Render loop and a window.
// Job   = single-dispatch headless app: Run() once to completion, then exit.
type Kind = "Scene" | "Job" | "Module";

// Mirrors `GitInfo` — the bit of git status shown beside a project. Null when it isn't a repo.
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

// Mirrors `LabView` — one entry of a collection, plus every copy of it on this machine.
//
// A non-empty `localPaths` means the entry *is* a project here: selectable, buildable, runnable,
// and `[0]` is the copy to open. Empty means it is still just a repository to download. There can
// be more than one copy (see the Rust side), and all of them matter — a project the sidebar shows
// under a collection must not also show as a loose project.
type Lab = {
  name: string;
  description: string;
  url: string;
  kind?: Kind;
  localPaths: string[];
};

/// The copy of a collection entry to open, or undefined when it is not on this machine.
function labPath(lab: Lab): string | undefined {
  return lab.localPaths[0];
}

// What a collection holds — fixed at creation, enforced when adding.
type Contents = "projects" | "modules" | "mixed";

// Mirrors `ModuleView` — one module this machine can offer a project. `id` is the exact string
// stored under "modules" in koral.json; `path` is set only for the user's own module projects.
// `hasSource` says whether a debugger can step into it from here.
type ModuleView = {
  id: string;
  name: string;
  source: "project" | "framework";
  path?: string;
  hasSource: boolean;
};

// Mirrors `CollectionView` — a subscribed collection, already flattened: either its fetched
// contents or the error fetching it produced. The error is per-collection so one bad URL never
// blanks the rest.
type CollectionView = {
  url: string;
  title: string;
  description: string;
  contents: Contents;
  labs: Lab[];
  error: string | null;
  /// Set when this subscription points at a collection the user authors locally — the same
  /// collection, reached by its published URL. The sidebar then shows only the authored copy.
  authoredPath?: string;
};

// Mirrors `AuthoredCollection` — a collection the user is building locally. `path` is
// machine-local; the rest is read from its koral-collection.json.
type AuthoredCollection = {
  path: string;
  title: string;
  description: string;
  /// What it holds — decides which of the user's projects may be added.
  contents: Contents;
  labCount: number;
  labs: Lab[];
  git: GitInfo | null;
};

// The noun a collection's UI uses for its entries, so a module registry doesn't call its modules
// "projects" in empty states and buttons.
function entryWord(contents: Contents | undefined): string {
  return contents === "modules" ? "module" : contents === "mixed" ? "entry" : "project";
}

// Whether a project of this kind may be added to a collection with these contents. Mirrors the
// backend's Contents::accepts, which enforces it for real — this copy only filters the pickers.
function contentsAccepts(contents: Contents | undefined, kind: Kind): boolean {
  if (contents === "modules") return kind === "Module";
  if (contents === "mixed") return true;
  return kind !== "Module";
}

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

// Mirrors `InstalledFramework`. `local` marks a build from source, registered from a path: it has
// no version (a working tree has nothing stable to number), so `version` carries its *pin* —
// what a project writes to target it — while `name` is what to show a human.
type InstalledFramework = {
  version: string;
  name: string;
  platform: string;
  path: string;
  sizeBytes: number;
  local: boolean;
  /// The tree it was built from, when the Hub could find it. This is what a debugger steps into.
  sourceDir?: string;
  /// CMAKE_BUILD_TYPE of the build it was installed from — a Release build has no debug info.
  buildType?: string;
};

// Mirrors `DetectedSource` — what the Hub can work out about an install prefix before it is
// registered, so the Add dialog can show it rather than asking blind.
type DetectedSource = { sourcePath: string; buildPath: string; buildType: string };

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
  // moduleDirectories is carried through but not edited here (like imguiIni): bare module names
  // resolve against the framework's own modules with no configuration, and pointing at a custom
  // build directory is a hand edit.
  paths: { assetDirectories: string[]; shaderDirectories: string[]; moduleDirectories?: string[] };
  // Koral modules the runtime loads for this project — optional engine features shipped as
  // shared libraries, by bare name ("koral-camera") or project-relative path. Order is
  // irrelevant: the runtime sorts them by their declared dependencies. Absent and empty mean
  // the same thing; saveSettings drops the key when the list is empty.
  modules?: string[];
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

// Build and run events carry the project they belong to, because each project has its own console.
// Mirrors `builder::Line` and `builder::Finished`; `project` is the path the job was started with.
type ConsoleLine = { project: string; text: string };
type Finished = { project: string; success: boolean; error: string | null };
type InstallProgress = { version: string; downloaded: number; total: number };
type InstallFinished = { version: string; success: boolean; error: string | null };

// What is selected in the sidebar, and so what the detail panel shows.
//
// A collection entry that exists on this machine is selected as the *project* it is — the panel
// for a lab you have downloaded is the panel for the project, with everything that implies. Only
// an entry with nothing on disk is a `lab`, whose panel offers to download it.
type Selection =
  | { kind: "project"; path: string }
  | { kind: "collection"; key: string }
  | { kind: "lab"; key: string; url: string };

function sameSelection(a: Selection | null, b: Selection | null): boolean {
  if (!a || !b || a.kind !== b.kind) return false;
  if (a.kind === "project") return a.path === (b as typeof a).path;
  if (a.kind === "collection") return a.key === (b as typeof a).key;
  const lab = b as Extract<Selection, { kind: "lab" }>;
  return a.kind === "lab" && a.key === lab.key && a.url === lab.url;
}

// One collection in the sidebar, whether it is one the user authors or one they subscribe to.
// Keyed by path (authored) or URL (subscribed) — both unique, and never confusable.
type SidebarCollection = {
  key: string;
  title: string;
  subtitle: string;
  contents: Contents;
  labs: Lab[];
  error: string | null;
  authored: AuthoredCollection | null;
  subscribed: CollectionView | null;
};

function rgb([r, g, b]: [number, number, number]): string {
  return `rgb(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)})`;
}

function mb(bytes: number): string {
  return `${(bytes / 1_048_576).toFixed(1)} MB`;
}

// Does this `frameworkVersion` name a build from source rather than a release? Mirrors the
// backend's `framework::source_pin` — "source", or "source:<name>" for a specific one.
function isSourcePin(version: string): boolean {
  return version === "source" || version.startsWith("source:");
}

// How a framework pin reads in the UI. A source build has no version to show, so it is named by
// what it is; a release shows its number.
function frameworkLabel(version: string): string {
  if (!version) return "no framework";
  if (version === "source") return "source build";
  if (version.startsWith("source:")) return `source · ${version.slice("source:".length)}`;
  return `koral ${version}`;
}

// Can a debugger step into framework code built this way? Debug obviously; RelWithDebInfo too —
// it is optimised, but the debug info is there, and calling it "no debug info" would be untrue.
// Anything else (Release, MinSizeRel) carries none.
function carriesDebugInfo(buildType: string | undefined): boolean {
  return buildType === "Debug" || buildType === "RelWithDebInfo";
}

// What to say about a source build's build type, given it is the one thing that decides whether
// debugging can step into engine code.
function buildTypeHint(buildType: string): string {
  if (buildType === "Debug") {
    return "Built with debug info — debugging can step into engine code";
  }
  if (buildType === "RelWithDebInfo") {
    return (
      "Built optimised, but with debug info — debugging can still step into engine code, " +
      "though inlining makes it jump around"
    );
  }
  return `Built ${buildType}: no debug info, so a crash inside the framework won't open its source`;
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

// Widest window either dimension may be dragged (or typed) to. 8K covers every real display and
// then some, and caps a typo that would otherwise ask the runtime for a surface it cannot make.
const SIZE_MIN = 0;
const SIZE_MAX = 8192;

const clampSize = (v: number) => Math.min(SIZE_MAX, Math.max(SIZE_MIN, Math.round(v) || 0));

// Make a number field scrub: drag left/right to change its value, the way a DAW or a 3D tool does.
//
// A plain click must still put a caret in the box, so the drag only takes over once the pointer
// has actually travelled — under that threshold the gesture is left alone and behaves as a click.
// Holding shift drops to 1px-per-unit for fine adjustment.
function scrubNumber(get: () => number, set: (value: number) => void) {
  return (e: PointerEvent & { currentTarget: HTMLInputElement }) => {
    if (e.button !== 0) return;
    const input = e.currentTarget;
    const startX = e.clientX;
    const startValue = get();
    let dragging = false;

    const onMove = (ev: PointerEvent) => {
      const dx = ev.clientX - startX;
      if (!dragging) {
        if (Math.abs(dx) < 4) return; // still indistinguishable from a click
        dragging = true;
        // Hand the gesture to the drag: no caret, no text selection, and a cursor that says so.
        input.blur();
        document.body.style.cursor = "ew-resize";
        document.body.style.userSelect = "none";
      }
      set(clampSize(startValue + dx * (ev.shiftKey ? 1 : 8)));
    };

    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  };
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

type SelectOption = { value: string; label: string };

// A dropdown drawn by us rather than by the platform.
//
// A native <select> renders its popup as an OS widget — GTK here — which takes the *system*
// theme's colours, corners and metrics and cannot be styled from CSS at all. Stripping the
// trigger's appearance (see `.select-trigger`) fixes the closed state but the open list still
// arrives looking like something from another application. The only way to make it match is to
// stop using the native control for the list.
//
// Behaves as a combobox is expected to: the trigger keeps focus while the list is open and drives
// it from the keyboard, so nothing here relies on the pointer.
function Select(props: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  /** Shown when the current value is not among the options — never silently blank. */
  placeholder?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = createSignal(false);
  const [active, setActive] = createSignal(0);
  const [anchor, setAnchor] = createSignal({ left: 0, width: 0, top: 0, bottom: 0, up: false });
  let trigger!: HTMLButtonElement;
  let list: HTMLDivElement | undefined;

  const selectedIndex = () => props.options.findIndex((o) => o.value === props.value);
  const label = () => props.options[selectedIndex()]?.label ?? props.placeholder ?? props.value;

  // Positioned against the viewport, not the trigger's parent: these live inside scrolling panels
  // and modals, and an absolutely-positioned list would be clipped by the first one with
  // `overflow` set. Flips above the trigger when there is no room below.
  function place() {
    const r = trigger.getBoundingClientRect();
    const wanted = Math.min(288, props.options.length * 42 + 12);
    const below = window.innerHeight - r.bottom - 12;
    const up = below < wanted && r.top > below;
    setAnchor({
      left: r.left,
      width: r.width,
      top: r.bottom + 6,
      bottom: window.innerHeight - r.top + 6,
      up,
    });
  }

  function openMenu() {
    if (props.disabled || props.options.length === 0) return;
    place();
    setActive(Math.max(0, selectedIndex()));
    setOpen(true);
  }

  function choose(index: number) {
    const option = props.options[index];
    if (option) props.onChange(option.value);
    setOpen(false);
    trigger.focus();
  }

  // Keep the highlighted row on screen when arrowing through a long list.
  createEffect(() => {
    if (!open()) return;
    const row = list?.children[active()] as HTMLElement | undefined;
    row?.scrollIntoView({ block: "nearest" });
  });

  // The list is anchored to where the trigger *was*, so anything that moves it invalidates the
  // position. Closing is the honest response, and matches what a native popup does.
  createEffect(() => {
    if (!open()) return;
    const close = () => setOpen(false);
    window.addEventListener("resize", close);
    window.addEventListener("scroll", close, true);
    onCleanup(() => {
      window.removeEventListener("resize", close);
      window.removeEventListener("scroll", close, true);
    });
  });

  function onKeyDown(e: KeyboardEvent) {
    const last = props.options.length - 1;
    if (!open()) {
      if (["ArrowDown", "ArrowUp", "Enter", " "].includes(e.key)) {
        e.preventDefault();
        openMenu();
      }
      return;
    }
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setActive((i) => Math.min(last, i + 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setActive((i) => Math.max(0, i - 1));
        break;
      case "Home":
        e.preventDefault();
        setActive(0);
        break;
      case "End":
        e.preventDefault();
        setActive(last);
        break;
      case "Enter":
      case " ":
        e.preventDefault();
        choose(active());
        break;
      case "Escape":
        e.preventDefault();
        setOpen(false);
        break;
      case "Tab":
        setOpen(false);
        break;
    }
  }

  return (
    <>
      <button
        type="button"
        ref={trigger}
        class="input select-trigger"
        disabled={props.disabled}
        aria-haspopup="listbox"
        aria-expanded={open()}
        onClick={() => (open() ? setOpen(false) : openMenu())}
        onKeyDown={onKeyDown}
      >
        <span class="select-value">{label()}</span>
      </button>

      <Show when={open()}>
        <div class="menu-backdrop" onPointerDown={() => setOpen(false)} />
        <div
          ref={list}
          class="select-menu"
          classList={{ "select-menu-up": anchor().up }}
          role="listbox"
          style={{
            left: `${anchor().left}px`,
            width: `${anchor().width}px`,
            ...(anchor().up
              ? { bottom: `${anchor().bottom}px` }
              : { top: `${anchor().top}px` }),
          }}
        >
          <For each={props.options}>
            {(option, i) => (
              <div
                class="select-option"
                classList={{
                  active: active() === i(),
                  selected: option.value === props.value,
                }}
                role="option"
                aria-selected={option.value === props.value}
                onPointerEnter={() => setActive(i())}
                onClick={() => choose(i())}
              >
                {option.label}
              </div>
            )}
          </For>
        </div>
      </Show>
    </>
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
  // are kept apart so the console can show them on separate tabs — and kept per project, keyed by
  // path, because a build belongs to the project it was started on and not to the app. Switching
  // projects switches consoles: the output of a build you started an hour ago is still there when
  // you come back to it, and two projects building at once never interleave.
  type ProjectConsole = {
    build: string;
    run: string;
    tab: "build" | "output";
    /** A job is in flight — this project's Run/Build are held, but nobody else's. */
    running: boolean;
    /** The job was a run (▶) rather than build-only, so we know to flip to the Output tab once
     *  its build succeeds. */
    wasRun: boolean;
  };
  const emptyConsole = (): ProjectConsole => ({
    build: "",
    run: "",
    tab: "build",
    running: false,
    wasRun: false,
  });
  const [consoles, setConsoles] = createStore<Record<string, ProjectConsole>>({});

  /** Is a build/run job in flight for this project? */
  const isRunning = (path: string) => !!consoles[path]?.running;

  // Output can arrive for a project whose console was never opened here (the app keeps printing
  // after its job finished, and the user may have closed the panel since), so every write starts
  // by making sure there is something to write into.
  const openConsoleFor = (path: string) => {
    if (!consoles[path]) setConsoles(path, emptyConsole());
  };

  // Each log is capped so a chatty app can't grow it unbounded.
  const LOG_CAP = 200_000;
  const appendCapped = (path: string, stream: "build" | "run", chunk: string) => {
    openConsoleFor(path);
    setConsoles(path, stream, (prev) => {
      const next = prev + chunk;
      return next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
    });
  };

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

  // Collections the user subscribes to. Fetched off the network on every open (a course can revise
  // its labs after subscribing), so the resource can be slow — but a single unreachable collection
  // surfaces as that row's error, not a rejected resource.
  const [collections, { refetch: refetchCollections }] = createResource<CollectionView[]>(() =>
    invoke("list_collections"),
  );
  // Add-collection dialog (subscribe to a remote collection to browse).
  const [showAddCollection, setAddingCollection] = createSignal(false);
  const [collectionUrl, setCollectionUrl] = createSignal("");
  const canAddCollection = () => !!collectionUrl().trim() && !busy();
  // The lab currently downloading (its git URL), so only its button shows the pending state.
  const [downloadingLab, setDownloadingLab] = createSignal<string | null>(null);

  // The frameworks dialog: installing releases and registering source builds. A dialog rather than
  // a place of its own, because it is something you go and do, then come back from.
  const [showFrameworks, setShowFrameworks] = createSignal(false);

  // Collections the user is authoring locally. Local repos, always readable.
  const [authored, { refetch: refetchAuthored }] = createResource<AuthoredCollection[]>(() =>
    invoke("list_authored_collections"),
  );
  // Create-collection dialog. Shares the create dialog's `location` (same default folder).
  // `collectionSeed` is the project a right-click started this from, which is then added to the
  // collection the moment it exists — that being the only way to make one now.
  const [showCreateCollection, setCreatingCollection] = createSignal(false);
  const [collectionName, setCollectionName] = createSignal("");
  const [collectionDescription, setCollectionDescription] = createSignal("");
  const [collectionContents, setCollectionContents] = createSignal<Contents>("projects");
  const [collectionSeed, setCollectionSeed] = createSignal<RecentProject | null>(null);
  const canCreateCollection = () => !!collectionName().trim() && !!location() && !busy();

  // "Add to an existing collection": the project being filed, and which collection to file it in.
  // A project with no remote is published first, so this also carries the publish fields.
  const [addToCollection, setAddToCollection] = createSignal<RecentProject | null>(null);
  const [targetCollection, setTargetCollection] = createSignal("");
  const [addProjectHost, setAddProjectHost] = createSignal("");
  const [addProjectRepoName, setAddProjectRepoName] = createSignal("");
  const [addProjectPrivate, setAddProjectPrivate] = createSignal(false);
  const [labDescription, setLabDescription] = createSignal("");

  // Add-by-URL dialog, for putting a repository that is not one of your projects into a collection.
  const [addUrlTo, setAddUrlTo] = createSignal<AuthoredCollection | null>(null);
  const [labUrl, setLabUrl] = createSignal("");
  const [labName, setLabName] = createSignal("");

  // Whether the project being filed still needs a remote before it can be a submodule.
  const seedNeedsPublish = (p: RecentProject | null) => !!p && !p.git?.remote;
  // The collections a project may actually go into: those whose contents accept its kind.
  const eligibleCollections = (p: RecentProject | null) =>
    (authored() ?? []).filter((c) => !!p && contentsAccepts(c.contents, p.kind));
  const canAddToCollection = () => {
    const p = addToCollection();
    if (busy() || !p || !targetCollection()) return false;
    if (seedNeedsPublish(p)) return !!addProjectHost() && !!addProjectRepoName().trim();
    return true;
  };
  const canCreateWithSeed = () => {
    if (!canCreateCollection()) return false;
    const p = collectionSeed();
    if (!p || !seedNeedsPublish(p)) return true;
    return !!addProjectHost() && !!addProjectRepoName().trim();
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

  // Every framework a project could target: this machine's source builds first (someone who has
  // one is working on the framework itself), then what is installed, then what GitHub offers —
  // pinning a release you have not downloaded is legitimate, since the first build fetches it.
  type FrameworkChoice = { value: string; label: string; installed: boolean; local: boolean };
  const frameworkChoices = (): FrameworkChoice[] => {
    const out: FrameworkChoice[] = [];
    const seen = new Set<string>();
    const add = (choice: FrameworkChoice) => {
      if (seen.has(choice.value)) return;
      seen.add(choice.value);
      out.push(choice);
    };

    for (const f of installed() ?? []) {
      if (!f.local) continue;
      add({ value: f.version, label: `Source build — ${f.name}`, installed: true, local: true });
    }
    for (const f of installed() ?? []) {
      if (f.local) continue;
      add({ value: f.version, label: `koral ${f.version}`, installed: true, local: false });
    }
    const releases = (available() ?? [])
      .filter((f) => !seen.has(f.version))
      .sort((a, b) => b.version.localeCompare(a.version, undefined, { numeric: true }));
    for (const f of releases) {
      add({
        value: f.version,
        label: `koral ${f.version}${f.installed ? "" : " (not installed — will download)"}`,
        installed: f.installed,
        local: false,
      });
    }
    return out;
  };

  // Path of the project currently being opened, so only its buttons show the pending state.
  const [opening, setOpening] = createSignal<string | null>(null);

  // The framework versions offered in a project's settings: the usual choices, plus the project's
  // own pin if it isn't among them (offline, an old release GitHub no longer lists, or a source
  // build that is no longer registered), so the dropdown always shows what it is actually on.
  const projectFwChoices = (): FrameworkChoice[] => {
    const list = frameworkChoices();
    const cur = draft.cfg?.frameworkVersion;
    if (cur && !list.some((v) => v.value === cur)) {
      const label = !isSourcePin(cur)
        ? `koral ${cur}`
        : sourcePinAmbiguous(cur)
          ? "source build (several registered — pick one)"
          : `${frameworkLabel(cur)} (not registered here)`;
      return [{ value: cur, label, installed: false, local: isSourcePin(cur) }, ...list];
    }
    return list;
  };

  // Remove-project confirmation: the project awaiting confirmation, and whether to erase its files.
  const [removing, setRemoving] = createSignal<RecentProject | null>(null);
  const [deleteFiles, setDeleteFiles] = createSignal(false);

  // --- Sidebar: one list of everything, projects and the collections that group them ---

  const [selected, setSelected] = createSignal<Selection | null>(null);
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set());
  const [filter, setFilter] = createSignal("");
  // The Import menu under the list: bringing in work that already exists, from either end of it —
  // one project, or a whole collection.
  const [importMenu, setImportMenu] = createSignal(false);
  // Right-click menu: what it acts on, and where to draw it.
  const [contextMenu, setContextMenu] = createSignal<{
    x: number;
    y: number;
    project?: RecentProject;
    collection?: SidebarCollection;
  } | null>(null);

  const isExpanded = (key: string) => expanded().has(key);
  function toggleExpanded(key: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(key) ? next.delete(key) : next.add(key);
      return next;
    });
  }

  // The sidebar's top-level groups. A project's kind is what separates it from its neighbours, so
  // the list says it once per group instead of tagging every row with it.
  const [collapsedGroups, setCollapsedGroups] = createSignal<Set<string>>(new Set());
  // While filtering, a collapsed group would hide its own matches — so filtering opens everything
  // and the result reads as one flat answer.
  const groupOpen = (key: string) => !!filter().trim() || !collapsedGroups().has(key);
  function toggleGroup(key: string) {
    setCollapsedGroups((prev) => {
      const next = new Set(prev);
      next.has(key) ? next.delete(key) : next.add(key);
      return next;
    });
  }

  const KIND_GROUPS = [
    ["Scene", "Scenes"],
    ["Job", "Jobs"],
    ["Module", "Modules"],
  ] as const;

  // Empty groups are left out rather than shown at zero: a permanent "Jobs 0" is noise on a
  // machine that has never made one.
  const projectGroups = () =>
    KIND_GROUPS.map(([kind, label]) => ({
      key: kind,
      label,
      items: looseProjects().filter((p) => p.kind === kind),
    })).filter((group) => group.items.length > 0);

  // Every collection, authored and subscribed, in one shape the sidebar can render uniformly.
  //
  // A subscription to a collection you author is the *same* collection — an easy thing to end up
  // with, since pasting your own published link is how you check it worked. It is listed once, as
  // the authored copy, which can do everything the subscription can and more. The subscription is
  // still there and still cancellable, from that collection's own panel.
  const sidebarCollections = (): SidebarCollection[] => [
    ...(authored() ?? []).map((c) => ({
      key: c.path,
      title: c.title,
      subtitle: c.path,
      contents: c.contents,
      labs: c.labs,
      error: null,
      authored: c,
      subscribed: null,
    })),
    ...(collections() ?? [])
      .filter((c) => !c.authoredPath)
      .map((c) => ({
        key: c.url,
        title: c.title || c.url,
        subtitle: repoLabel(c.url),
        contents: c.contents,
        labs: c.labs,
        error: c.error,
        authored: null,
        subscribed: c,
      })),
  ];

  /// The subscription that points at an authored collection, when the user has one.
  const subscriptionFor = (path: string) =>
    (collections() ?? []).find((c) => c.authoredPath === path);

  // Projects that a collection already accounts for. Listing one twice — once loose, once under
  // the collection it belongs to — would make the same folder look like two projects.
  //
  // *Every* copy is claimed, not just the one the collection opens: with your own collection you
  // can easily have both the project you added and a copy downloaded from it, and the leftover
  // would otherwise reappear as an unrelated loose project.
  const claimedPaths = () => {
    const claimed = new Set<string>();
    for (const c of sidebarCollections()) {
      for (const lab of c.labs) for (const path of lab.localPaths) claimed.add(path);
    }
    return claimed;
  };

  const matches = (text: string) => text.toLowerCase().includes(filter().trim().toLowerCase());

  const looseProjects = () =>
    (projects() ?? [])
      .filter((p) => !claimedPaths().has(p.path))
      .filter((p) => !filter().trim() || matches(p.name) || matches(p.path));

  // A collection stays visible while filtering if it matches itself or holds something that does;
  // its entry list narrows to the matches, so a filter reads as one flat answer.
  const visibleLabs = (c: SidebarCollection) =>
    !filter().trim() ? c.labs : c.labs.filter((l) => matches(l.name) || matches(l.url));
  const visibleCollections = () =>
    sidebarCollections().filter(
      (c) => !filter().trim() || matches(c.title) || visibleLabs(c).length > 0,
    );
  // While filtering, a collection that only matched through its entries is opened, so the hit is
  // actually on screen rather than hidden behind a chevron.
  const showLabs = (c: SidebarCollection) =>
    isExpanded(c.key) || (!!filter().trim() && visibleLabs(c).length > 0);

  const projectAt = (path: string) => (projects() ?? []).find((p) => p.path === path);

  // A project selected from a collection may not be on the recent list at all — an authored
  // collection's entries are its own submodule checkouts, which nobody ever "opened". They are
  // still real projects, so the panel reads them from disk instead; this holds the last one read.
  const [offListProject, setOffListProject] = createSignal<RecentProject | null>(null);
  const projectOrOffList = (path: string) => {
    const known = projectAt(path);
    if (known) return known;
    const loaded = offListProject();
    return loaded?.path === path ? loaded : undefined;
  };
  const labProject = (lab: Lab) => {
    const path = labPath(lab);
    return path ? projectOrOffList(path) : undefined;
  };

  const selectedProject = (): RecentProject | undefined => {
    const sel = selected();
    return sel?.kind === "project" ? projectOrOffList(sel.path) : undefined;
  };
  // The console the bottom panel shows: the selected project's, once it has something in it. Only
  // a project has one, so selecting a collection puts the panel away — a lab that has been
  // downloaded selects as its project, so its console shows here like any other.
  const shownConsole = () => {
    const sel = selected();
    if (sel?.kind !== "project") return undefined;
    const c = consoles[sel.path];
    return c && (c.build || c.run) ? { path: sel.path, ...c } : undefined;
  };

  const selectedCollection = (): SidebarCollection | undefined => {
    const sel = selected();
    return sel?.kind === "collection"
      ? sidebarCollections().find((c) => c.key === sel.key)
      : undefined;
  };
  const selectedLab = (): { collection: SidebarCollection; lab: Lab } | undefined => {
    const sel = selected();
    if (sel?.kind !== "lab") return undefined;
    const collection = sidebarCollections().find((c) => c.key === sel.key);
    const lab = collection?.labs.find((l) => l.url === sel.url);
    return collection && lab ? { collection, lab } : undefined;
  };

  // Selecting a project loads its koral.json into the editor on the right, so switching selection
  // is what opens settings — there is no separate panel to go and find.
  function select(next: Selection) {
    if (sameSelection(next, selected())) return;
    // Never lose an edit to a click: switching away from a project with unsaved settings asks
    // first. (This is the only place a selection changes, so the guard cannot be bypassed.)
    if (settingsDirty()) {
      setPendingSelection(next);
      return;
    }
    applySelection(next);
  }

  function applySelection(next: Selection) {
    setSelected(next);
    if (next.kind === "project") loadProjectSettings(next.path);
    else closeProjectSettings();
  }

  // --- Project settings, edited inline in the detail panel ---

  const [settingsPath, setSettingsPath] = createSignal<string | null>(null);
  const [draft, setDraft] = createStore<{ cfg: ProjectConfig | null }>({ cfg: null });
  // The config as loaded, so "dirty" is a fact rather than a guess and Revert has something to
  // revert to.
  const [savedConfig, setSavedConfig] = createSignal<string>("");
  // The modules this machine can offer the selected project, refreshed on every selection (the set
  // changes as the user creates, downloads or removes module projects).
  const [availableModules, setAvailableModules] = createSignal<ModuleView[]>([]);
  // A selection waiting on the user to decide what to do with unsaved settings.
  const [pendingSelection, setPendingSelection] = createSignal<Selection | null>(null);

  const settingsDirty = () =>
    !!draft.cfg && !!savedConfig() && JSON.stringify(draft.cfg) !== savedConfig();

  async function loadProjectSettings(path: string) {
    setError(null);
    closeProjectSettings();
    setSettingsPath(path);
    try {
      // A collection's own checkout is not on the recent list, so its header details have to be
      // read from the folder. Skipped for the ordinary case, where the list already has them.
      if (!projectAt(path)) {
        const details = await invoke<RecentProject>("project_details", { path });
        if (settingsPath() !== path) return;
        setOffListProject(details);
      }
      const cfg = await invoke<ProjectConfig>("project_config", { path });
      // The backend omits an empty modules list entirely; the editor needs an array to exist so
      // its store paths ("cfg", "modules", i) have something to write into.
      cfg.modules ??= [];
      // The selection may have moved on while this was in flight — don't clobber the new one.
      if (settingsPath() !== path) return;
      setDraft("cfg", cfg);
      setSavedConfig(JSON.stringify(cfg));
      // What this machine can offer depends on the project's framework, so it is fetched per
      // project rather than once. Local-only, so it is quick.
      const mods = await invoke<ModuleView[]>("available_modules", {
        frameworkVersion: cfg.frameworkVersion,
      });
      if (settingsPath() === path) setAvailableModules(mods);
    } catch (e) {
      setError(String(e));
    }
  }

  function closeProjectSettings() {
    setSettingsPath(null);
    setDraft("cfg", null);
    setSavedConfig("");
    setAvailableModules([]);
  }

  function revertSettings() {
    const saved = savedConfig();
    if (saved) setDraft("cfg", JSON.parse(saved));
  }

  /** Is this module id in the project's list? */
  const hasModule = (id: string) => (draft.cfg?.modules ?? []).includes(id);

  /** Tick / untick a module. Order is irrelevant — the runtime sorts by declared dependencies. */
  function toggleModule(id: string) {
    setDraft("cfg", "modules", (mods) => {
      const current = mods ?? [];
      return current.includes(id) ? current.filter((m) => m !== id) : [...current, id];
    });
  }

  /** Entries in the file that the picker cannot show, because this machine doesn't have them. */
  const unregisteredModules = () => {
    const offered = new Set((availableModules() ?? []).map((m) => m.id));
    return (draft.cfg?.modules ?? []).filter((m) => !offered.has(m));
  };

  const sourceBuilds = () => (installed() ?? []).filter((f) => f.local);

  // The registered source build a pin resolves to, when the selected project targets one — what
  // the panel needs to say whether debugging can step into framework code. Mirrors the backend's
  // `local::resolve_pin`, including its one refusal: a bare `source` with several builds
  // registered is ambiguous, and the build would fail rather than pick one.
  const pinnedSourceBuild = (version: string): InstalledFramework | undefined => {
    if (!isSourcePin(version)) return undefined;
    const sources = sourceBuilds();
    if (version === "source") return sources.length === 1 ? sources[0] : undefined;
    return sources.find((f) => f.name === version.slice("source:".length));
  };
  const sourcePinAmbiguous = (version: string) =>
    version === "source" && sourceBuilds().length > 1;

  async function saveSettings(e?: Event) {
    e?.preventDefault();
    const path = settingsPath();
    const config = draft.cfg;
    if (!path || !config) return false;

    setBusy(true);
    setError(null);
    try {
      // Entries the user added and left blank would be launch-time "module not found" errors;
      // drop them here, where the mistake is visible, rather than there.
      const cleaned = {
        ...config,
        modules: (config.modules ?? []).map((m) => m.trim()).filter(Boolean),
      };
      await invoke("save_project_config", { path, config: cleaned });
      setSavedConfig(JSON.stringify(draft.cfg));
      await refetch();
      return true;
    } catch (e) {
      setError(String(e));
      return false;
    } finally {
      setBusy(false);
    }
  }

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
      await listen<ConsoleLine>("build-output", (e) =>
        appendCapped(e.payload.project, "build", e.payload.text),
      ),
    );
    unlisten.push(
      await listen<ConsoleLine>("run-output", (e) =>
        appendCapped(e.payload.project, "run", e.payload.text),
      ),
    );
    unlisten.push(
      await listen<Finished>("build-finished", (e) => {
        const { project, success, error: failure } = e.payload;
        appendCapped(project, "build", success ? "\n✓ done\n" : `\n✗ ${failure ?? "failed"}\n`);
        setConsoles(project, "running", false);
        // A successful run has now launched — show its Output tab. A failed one (or a build-only)
        // stays on Build so the errors are in view.
        if (success && consoles[project].wasRun) setConsoles(project, "tab", "output");
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

  // Open the first project as soon as there is one, so the app never starts on an empty panel
  // asking the user to click something to see anything at all.
  createEffect(() => {
    if (!selected() && !projects.loading && (projects()?.length ?? 0) > 0) {
      applySelection({ kind: "project", path: projects()![0].path });
    }
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

  // --- Frameworks built from source ---
  const [showAddLocal, setAddingLocal] = createSignal(false);
  const [localPath, setLocalPath] = createSignal("");
  const [localSource, setLocalSource] = createSignal("");
  // What the Hub worked out about the prefix that was typed/picked, so the dialog can show the
  // source tree it found instead of registering a build that cannot be stepped into.
  const [detected, setDetected] = createSignal<DetectedSource | null>(null);
  const canAddLocal = () => !!localPath().trim() && !busy();

  // Probe whatever prefix is in the box. Debounced by being called only on blur/Browse rather than
  // on every keystroke — it walks the filesystem.
  async function probeLocalPath(path: string) {
    const trimmed = path.trim();
    if (!trimmed) {
      setDetected(null);
      return;
    }
    try {
      setDetected(await invoke<DetectedSource>("detect_framework_source", { path: trimmed }));
    } catch {
      // Purely advisory — a prefix that cannot be probed is still registrable, and `add` is what
      // says whether it is a real SDK.
      setDetected(null);
    }
  }

  async function browseLocalFramework() {
    const picked = await open({ directory: true, multiple: false, title: "Koral SDK install prefix" });
    if (typeof picked === "string") {
      setLocalPath(picked);
      await probeLocalPath(picked);
    }
  }

  async function browseLocalSource() {
    const picked = await open({
      directory: true,
      multiple: false,
      title: "Koral source tree",
      defaultPath: localSource() || detected()?.sourcePath || undefined,
    });
    if (typeof picked === "string") setLocalSource(picked);
  }

  function openAddLocal() {
    setError(null);
    setLocalPath("");
    setLocalSource("");
    setDetected(null);
    setAddingLocal(true);
  }

  async function submitAddLocal(e: Event) {
    e.preventDefault();
    if (!canAddLocal()) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("add_local_framework", {
        req: { path: localPath().trim(), sourcePath: localSource().trim() },
      });
      await Promise.all([refetchInstalled(), refetchAvailable()]);
      setAddingLocal(false);
    } catch (e) {
      // Stay open: the message names what is wrong with the directory, and the path is right here.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function removeSourceBuild(name: string) {
    setError(null);
    try {
      await invoke("remove_local_framework", { name });
      await Promise.all([refetchInstalled(), refetchAvailable()]);
    } catch (e) {
      setError(String(e));
    }
  }

  // Point a registered build at its source tree by hand — the fix for a build directory that has
  // since been cleaned, which is what leaves a source build undebuggable.
  async function locateSource(fw: InstalledFramework) {
    const picked = await open({
      directory: true,
      multiple: false,
      title: `Source tree for ${fw.name}`,
      defaultPath: fw.sourceDir || undefined,
    });
    if (typeof picked !== "string") return;
    setError(null);
    try {
      await invoke("set_framework_source", { name: fw.name, sourcePath: picked });
      await refetchInstalled();
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
      const created = await invoke<RecentProject>("create_project", {
        req: { location: location(), name: trimmed, kind: kind() },
      });
      await refetch();
      setCreating(false);
      setName("");
      // Land on what was just made — the next thing anyone does is open its settings or run it.
      applySelection({ kind: "project", path: created.path });
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
    setImportMenu(false);
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
      const imported = await invoke<RecentProject>("import_project", {
        req: { url, location: location() },
      });
      await refetch();
      setImporting(false);
      setGitUrl("");
      applySelection({ kind: "project", path: imported.path });
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
    setImportMenu(false);
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
      const added = await invoke<CollectionView>("add_collection", { url });
      await refetchCollections();
      setAddingCollection(false);
      setCollectionUrl("");
      setExpanded((prev) => new Set(prev).add(added.url));
      applySelection({ kind: "collection", key: added.url });
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
      // Only if *this* collection was showing. Unsubscribing from a collection you also author is
      // done from the authored one's own panel, which must stay put underneath you.
      if (sameSelection(selected(), { kind: "collection", key: url })) setSelected(null);
      await refetchCollections();
    } catch (e) {
      setError(String(e));
    }
  }

  // Download a lab into the default project folder as a fresh project, then select it. No dialog —
  // a lab is meant to be one click from the collection into your workspace.
  async function downloadLab(url: string) {
    setError(null);
    setDownloadingLab(url);
    try {
      const loc = await ensureLocation();
      const project = await invoke<RecentProject>("download_lab", { req: { url, location: loc } });
      // Both lists change: the project is new, and the collection entry now has a local path.
      await Promise.all([refetch(), refetchCollections(), refetchAuthored()]);
      applySelection({ kind: "project", path: project.path });
    } catch (e) {
      setError(String(e));
    } finally {
      setDownloadingLab(null);
    }
  }

  // --- Authoring a collection ---

  // Collections are made *from* a project: right-click one and file it. That keeps a collection
  // from ever existing as an empty shell nobody remembers making.
  async function openCreateCollection(seed: RecentProject) {
    setError(null);
    setContextMenu(null);
    setCollectionSeed(seed);
    setCollectionName("");
    setCollectionDescription("");
    // A module registry and a lab list are different things, and the project being filed says
    // which this is. Still editable, for a collection meant to hold both.
    setCollectionContents(seed.kind === "Module" ? "modules" : "projects");
    setLabDescription("");
    setAddProjectHost(accounts()?.[0]?.host ?? "");
    setAddProjectRepoName(seed.name);
    setAddProjectPrivate(false);
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
    const collectionTitle = collectionName().trim();
    const seed = collectionSeed();
    if (!collectionTitle || !location() || !seed) return;

    setBusy(true);
    setError(null);
    try {
      const created = await invoke<AuthoredCollection>("create_collection", {
        req: {
          location: location(),
          name: collectionTitle,
          description: collectionDescription(),
          contents: collectionContents(),
        },
      });
      // The collection exists now, so a failure adding the project leaves something real and
      // visible rather than nothing — and the error says what to retry.
      await invoke("add_project_to_collection", {
        req: {
          path: created.path,
          projectPath: seed.path,
          description: labDescription(),
          // A project with a remote is added straight from it; only a local-only one is published.
          host: seed.git?.remote ? "" : addProjectHost(),
          repoName: addProjectRepoName().trim(),
          private: addProjectPrivate(),
        },
      });
      await Promise.all([refetchAuthored(), refetch()]);
      setCreatingCollection(false);
      setCollectionSeed(null);
      setExpanded((prev) => new Set(prev).add(created.path));
      applySelection({ kind: "collection", key: created.path });
    } catch (e) {
      setError(String(e));
      await Promise.all([refetchAuthored(), refetch()]);
    } finally {
      setBusy(false);
    }
  }

  function openAddToCollection(project: RecentProject) {
    setError(null);
    setContextMenu(null);
    setAddToCollection(project);
    setTargetCollection(eligibleCollections(project)[0]?.path ?? "");
    setLabDescription("");
    setAddProjectHost(accounts()?.[0]?.host ?? "");
    setAddProjectRepoName(project.name);
    setAddProjectPrivate(false);
  }

  async function submitAddToCollection(e: Event) {
    e.preventDefault();
    const project = addToCollection();
    if (!project || !canAddToCollection()) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("add_project_to_collection", {
        req: {
          path: targetCollection(),
          projectPath: project.path,
          description: labDescription(),
          host: project.git?.remote ? "" : addProjectHost(),
          repoName: addProjectRepoName().trim(),
          private: addProjectPrivate(),
        },
      });
      const key = targetCollection();
      await Promise.all([refetchAuthored(), refetch()]);
      setAddToCollection(null);
      setExpanded((prev) => new Set(prev).add(key));
    } catch (e) {
      // Stay in the dialog — the usual failures are not signed in, a private repo, or already added.
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function openAddUrl(c: AuthoredCollection) {
    setError(null);
    setLabUrl("");
    setLabName("");
    setLabDescription("");
    setAddUrlTo(c);
  }

  async function submitAddUrl(e: Event) {
    e.preventDefault();
    const c = addUrlTo();
    if (!c || !labUrl().trim() || busy()) return;

    setBusy(true);
    setError(null);
    try {
      await invoke("add_lab_to_collection", {
        req: { path: c.path, url: labUrl().trim(), name: labName(), description: labDescription() },
      });
      await refetchAuthored();
      setAddUrlTo(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // Path of the collection whose entry list is doing a remove/reorder, so its controls show a
  // pending state and don't fire twice mid-commit.
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
    setContextMenu(null);
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
      if (selected()?.kind === "collection") setSelected(null);
      await Promise.all([refetchAuthored(), refetch()]);
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

  /** Signed-in accounts as dropdown options — the same list wherever one has to be picked. */
  const accountOptions = () =>
    (accounts() ?? []).map((a) => ({ value: a.host, label: `${a.username}@${a.host}` }));

  function openPublish(target: PublishTarget, defaultName: string) {
    setError(null);
    setContextMenu(null);
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
      await Promise.all([refetchAuthored(), refetch()]);
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
    !!addToCollection() ||
    !!addUrlTo() ||
    !!removing() ||
    !!removingCollection() ||
    !!deviceLogin() ||
    !!publishTarget() ||
    !!pendingSelection() ||
    showAddLocal() ||
    showFrameworks() ||
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
      // The project is gone, and so is its console — nothing would ever show it again, and a
      // project later re-added at the same path must not inherit an old build's output.
      setConsoles(produce((all) => delete all[project.path]));
      if (selected()?.kind === "project") {
        setSelected(null);
        closeProjectSettings();
      }
      await Promise.all([refetch(), refetchCollections(), refetchAuthored()]);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // The release list is fetched once at startup, so a session that has been open a while would
  // otherwise show a stale list — and a failed fetch would stay failed. Re-check on every open.
  function openFrameworks() {
    setError(null);
    setShowFrameworks(true);
    refetchAvailable();
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
    setContextMenu(null);
    setDeleteFiles(false);
    setRemoving(project);
  }

  // Regenerates the IDE's build/run config before launching, so the editor is useful the moment
  // it opens. That means resolving (and possibly downloading) the SDK, hence the pending state.
  // `ideId` omitted → the backend uses the default from Settings.
  async function openInIde(path: string, ideId?: string) {
    setError(null);
    setContextMenu(null);
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

  // Start a job on one project: its console is reset to an empty Build tab, and only its own
  // buttons are held while it runs. Another project's console is left exactly as it was.
  async function startJob(path: string, command: "run_project" | "build_project") {
    setError(null);
    setContextMenu(null);
    setConsoles(path, { ...emptyConsole(), running: true, wasRun: command === "run_project" });
    try {
      await invoke(command, { path });
    } catch (e) {
      setConsoles(path, "running", false);
      setError(String(e));
    }
  }

  const runProject = (path: string) => startJob(path, "run_project");
  const buildProject = (path: string) => startJob(path, "build_project");

  // One project's row in the sidebar. Extracted because it is rendered once per kind group, and a
  // second copy would be a second place to keep the selection and context-menu wiring correct.
  const projectRow = (p: RecentProject) => (
    <button
      type="button"
      class="row row-project"
      classList={{ "row-active": sameSelection(selected(), { kind: "project", path: p.path }) }}
      onClick={() => select({ kind: "project", path: p.path })}
      onContextMenu={(e) => {
        select({ kind: "project", path: p.path });
        openContextMenu(e, { project: p });
      }}
    >
      <span class="row-swatch" style={{ "background-color": rgb(p.color) }}>
        {p.name.charAt(0).toUpperCase()}
      </span>
      <span class="row-meta">
        <span class="row-name">{p.name}</span>
        <span class="row-sub">{frameworkLabel(p.frameworkVersion)}</span>
      </span>
      {/* A build only shows its output on its own project's console, so a project building in the
          background would otherwise be invisible. This is the row saying it is still going. */}
      <Show when={isRunning(p.path)}>
        <span class="row-building" title="Building…" />
      </Show>
    </button>
  );

  // Right-click anywhere on a row, opening the menu at the pointer — `fitContextMenu` is what keeps
  // it inside the window.
  //
  // Always swallows the event, even when there is nothing to offer (an entry that is not on this
  // machine yet), so a right-click never falls through to the webview's own menu. And never opens
  // over the unsaved-changes prompt, which the same click may have just raised.
  function openContextMenu(
    e: MouseEvent,
    on: { project?: RecentProject; collection?: SidebarCollection },
  ) {
    e.preventDefault();
    if (pendingSelection() || (!on.project && !on.collection)) return;
    setContextMenu({ x: e.clientX, y: e.clientY, ...on });
  }

  // How big the menu is depends on what was right-clicked — a project offers three times what a
  // collection does, and a repo adds a header — so it can only be fitted once it exists. Measure on
  // mount and pull it back inside the window, rather than guessing a size up front and clamping to
  // that: guess low and a menu opened near the bottom hangs off the edge.
  function fitContextMenu(el: HTMLElement) {
    const margin = 8;
    const r = el.getBoundingClientRect();
    const x = Math.min(r.left, window.innerWidth - r.width - margin);
    const y = Math.min(r.top, window.innerHeight - r.height - margin);
    el.style.left = `${Math.max(margin, x)}px`;
    el.style.top = `${Math.max(margin, y)}px`;
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

      {/* --- Library: the sidebar of everything, and the panel for whatever is selected --- */}
      <div class="library">
        <aside class="sidebar">
          <div class="sidebar-tools">
            <input
              class="input sidebar-filter"
              type="search"
              value={filter()}
              placeholder="Filter…"
              onInput={(e) => setFilter(e.currentTarget.value)}
            />
          </div>

          <div class="sidebar-list">
            <Show when={!projects.loading} fallback={<p class="sidebar-empty muted-sm">Loading…</p>}>
              <Show
                when={looseProjects().length > 0 || visibleCollections().length > 0}
                fallback={
                  <p class="sidebar-empty muted-sm">
                    {filter().trim() ? "Nothing matches that filter." : "No projects yet."}
                  </p>
                }
              >
                {/* Scenes, Jobs and Modules, each its own collapsible group. The group heading
                    is what says a project's kind, so the rows themselves carry no kind tag. */}
                <For each={projectGroups()}>
                  {(group) => (
                    <>
                      <button
                        type="button"
                        class="group-row"
                        aria-expanded={groupOpen(group.key)}
                        onClick={() => toggleGroup(group.key)}
                      >
                        <span class="group-chevron" classList={{ open: groupOpen(group.key) }}>
                          ▸
                        </span>
                        <span class="group-label">{group.label}</span>
                        <span class="group-count">{group.items.length}</span>
                      </button>
                      <Show when={groupOpen(group.key)}>
                        <For each={group.items}>{(p) => projectRow(p)}</For>
                      </Show>
                    </>
                  )}
                </For>

                <Show when={visibleCollections().length > 0}>
                  <button
                    type="button"
                    class="group-row"
                    aria-expanded={groupOpen("collections")}
                    onClick={() => toggleGroup("collections")}
                  >
                    <span class="group-chevron" classList={{ open: groupOpen("collections") }}>
                      ▸
                    </span>
                    <span class="group-label">Collections</span>
                    <span class="group-count">{visibleCollections().length}</span>
                  </button>
                </Show>

                <Show when={groupOpen("collections")}>
                <For each={visibleCollections()}>
                  {(c) => (
                    <>
                      <button
                        type="button"
                        class="row row-collection"
                        classList={{ "row-active": sameSelection(selected(), { kind: "collection", key: c.key }) }}
                        onClick={() => {
                          select({ kind: "collection", key: c.key });
                          toggleExpanded(c.key);
                        }}
                        onContextMenu={(e) => {
                          select({ kind: "collection", key: c.key });
                          openContextMenu(e, { collection: c });
                        }}
                      >
                        <span class="row-chevron" classList={{ open: showLabs(c) }}>
                          ▸
                        </span>
                        <span class="row-meta">
                          <span class="row-name">{c.title}</span>
                          <span class="row-sub">
                            {c.error
                              ? "unavailable"
                              : `${c.labs.length} ${entryWord(c.contents)}${c.labs.length === 1 ? "" : "s"}`}
                          </span>
                        </span>
                        <Show when={!c.authored}>
                          <span class="row-kind">shared</span>
                        </Show>
                      </button>

                      <Show when={showLabs(c)}>
                        <For each={visibleLabs(c)}>
                          {(lab) => {
                            const local = () => labProject(lab);
                            const sel = (): Selection =>
                              labPath(lab)
                                ? { kind: "project", path: labPath(lab)! }
                                : { kind: "lab", key: c.key, url: lab.url };
                            return (
                              <button
                                type="button"
                                class="row row-lab"
                                classList={{ "row-active": sameSelection(selected(), sel()) }}
                                onClick={() => select(sel())}
                                onContextMenu={(e) => {
                                  select(sel());
                                  // Only a project has actions worth a menu; an entry not on
                                  // disk yet has exactly one, and the panel offers it.
                                  openContextMenu(e, { project: local() });
                                }}
                              >
                                <Show
                                  when={local()}
                                  fallback={<span class="row-swatch row-swatch-ghost">↓</span>}
                                >
                                  {(p) => (
                                    <span
                                      class="row-swatch"
                                      style={{ "background-color": rgb(p().color) }}
                                    >
                                      {p().name.charAt(0).toUpperCase()}
                                    </span>
                                  )}
                                </Show>
                                <span class="row-meta">
                                  <span class="row-name">{lab.name}</span>
                                  <span class="row-sub">
                                    {labPath(lab) ?? "not downloaded"}
                                  </span>
                                </span>
                                <Show when={lab.kind === "Module"}>
                                  <span class="row-kind">Module</span>
                                </Show>
                              </button>
                            );
                          }}
                        </For>
                      </Show>
                    </>
                  )}
                </For>
                </Show>
              </Show>
            </Show>
          </div>

          {/* Everything that belongs to the Hub rather than to any one project, under the list and
              out of the way of the work. Each is a round icon that grows its label on hover — except
              New Project, which wears its label until a neighbour is pointed at, because starting
              one is what this panel is for. */}
          <div class="sidebar-foot">
            <button class="btn btn-ghost btn-foot" onClick={openFrameworks}>
              {/* A package: the framework as a thing you install. */}
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
                <path
                  d="M8 1.7 14 5v6l-6 3.3L2 11V5z"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linejoin="round"
                />
                <path
                  d="M2 5l6 3.3L14 5M8 8.3v6"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linejoin="round"
                />
              </svg>
              <span class="btn-foot-label">Frameworks</span>
            </button>
            <button class="btn btn-ghost btn-foot" onClick={openSettingsPanel}>
              {/* Sliders rather than a gear: these are preferences, and it reads at 16px. */}
              <svg
                width="16"
                height="16"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.4"
                stroke-linecap="round"
                aria-hidden="true"
              >
                <path d="M2 4.5h12M2 11.5h12" />
                {/* Solid knobs, so they read on whatever the button's fill happens to be. */}
                <circle cx="10" cy="4.5" r="2" fill="currentColor" stroke="none" />
                <circle cx="5.5" cy="11.5" r="2" fill="currentColor" stroke="none" />
              </svg>
              <span class="btn-foot-label">Settings</span>
            </button>

            {/* One Import, two ends of the same act: a single project, or a whole collection. */}
            <div class="menu-wrap">
              <button
                class="btn btn-ghost btn-foot"
                classList={{ "menu-open": importMenu() }}
                onClick={() => setImportMenu(!importMenu())}
              >
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 16 16"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <path d="M8 2v7.6M5 6.9l3 3 3-3" />
                  <path d="M2.8 11.4v1.2c0 .7.5 1.2 1.2 1.2h8c.7 0 1.2-.5 1.2-1.2v-1.2" />
                </svg>
                <span class="btn-foot-label">Import</span>
              </button>
              <Show when={importMenu()}>
                <div class="menu-backdrop" onClick={() => setImportMenu(false)} />
                <div class="card-menu menu-left menu-up">
                  <button type="button" class="menu-item" onClick={openImport}>
                    Import from Git…
                  </button>
                  <button type="button" class="menu-item" onClick={openAddCollection}>
                    Add a collection by URL…
                  </button>
                  <p class="menu-note">
                    Collections of your own start from a project — right-click one.
                  </p>
                </div>
              </Show>
            </div>

            <button class="btn btn-primary btn-foot btn-foot-open" disabled={busy()} onClick={openCreate}>
              <svg
                width="16"
                height="16"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                aria-hidden="true"
              >
                <path d="M8 3v10M3 8h10" />
              </svg>
              <span class="btn-foot-label">New Project</span>
            </button>
          </div>
        </aside>

        <section class="detail">
          <Show when={error() && !anyModalOpen()}>
            <p class="error">{error()}</p>
          </Show>

          {/* --- A project: everything about it, and every setting it has --- */}
          <Show when={selectedProject()}>
            {(p) => (
              <>
                <header class="detail-head">
                  <span class="detail-swatch" style={{ "background-color": rgb(p().color) }}>
                    {p().name.charAt(0).toUpperCase()}
                  </span>
                  <div class="detail-title-block">
                    <h1 class="detail-title">{p().name}</h1>
                    <p class="detail-path" title={p().path}>
                      {p().path}
                    </p>
                  </div>
                  <div class="detail-actions">
                    {/* A module has no app to start — it runs inside projects that list it — so
                        offering ▶ would only lead to the backend's refusal. Build stays. */}
                    <Show when={p().kind !== "Module"}>
                      <button
                        class="btn btn-primary"
                        disabled={isRunning(p().path)}
                        onClick={() => runProject(p().path)}
                      >
                        {isRunning(p().path) ? "Running…" : "▶ Run"}
                      </button>
                    </Show>
                    <button
                      class="btn btn-ghost"
                      title="Build only (compile without running)"
                      disabled={isRunning(p().path)}
                      onClick={() => buildProject(p().path)}
                    >
                      Build
                    </button>
                    <Show when={defaultIde()}>
                      <button
                        class="btn btn-ghost"
                        title={`Open in ${defaultIde()!.name} (${defaultIde()!.command}) — change the default in Settings`}
                        disabled={opening() !== null}
                        onClick={() => openInIde(p().path)}
                      >
                        {opening() === p().path ? "Opening…" : `Open in ${defaultIde()!.name}`}
                      </button>
                    </Show>
                    <button
                      class="btn btn-ghost btn-icon"
                      title="More actions"
                      onClick={(e) => openContextMenu(e, { project: p() })}
                    >
                      ⋮
                    </button>
                  </div>
                </header>

                <div class="badge-row">
                  <span
                    class="project-kind"
                  >
                    {p().kind}
                  </span>
                  <span class="project-fw">{frameworkLabel(p().frameworkVersion)}</span>
                  <Show when={p().git}>
                    <span
                      class="project-git"
                      title={
                        (p().git!.remote ?? "local git repository") +
                        (p().git!.dirty ? " · uncommitted changes" : "")
                      }
                    >
                      <span class="git-branch-name">{p().git!.branch ?? "detached"}</span>
                      <Show when={p().git!.dirty}>
                        <span class="git-dot">●</span>
                      </Show>
                    </span>
                  </Show>
                  <Show when={p().git?.remote}>
                    <span class="detail-remote" title={p().git!.remote!}>
                      {repoLabel(p().git!.remote!)}
                    </span>
                  </Show>
                </div>

                <Show when={draft.cfg} fallback={<p class="muted">Loading settings…</p>}>
                  <form class="settings" onSubmit={saveSettings}>
                    <section class="panel">
                      <h2 class="panel-title">Framework</h2>
                      <label class="field">
                        <span class="field-label">Builds against</span>
                        <Select
                          value={draft.cfg!.frameworkVersion}
                          options={projectFwChoices().map((v) => ({
                            value: v.value,
                            label: v.label,
                          }))}
                          onChange={(v) => setDraft("cfg", "frameworkVersion", v)}
                        />
                      </label>
                      <Show
                        when={isSourcePin(draft.cfg!.frameworkVersion)}
                        fallback={
                          <p class="field-hint">
                            Resolved to a prebuilt SDK for this platform and downloaded on the
                            first build. Recorded in <code>koral.json</code>, so it travels with
                            the project.
                          </p>
                        }
                      >
                        {/* A source build is the one framework a debugger can step into, so this
                            is where to say whether it actually can. */}
                        <Show
                          when={pinnedSourceBuild(draft.cfg!.frameworkVersion)}
                          fallback={
                            <p class="field-hint field-bad">
                              {sourcePinAmbiguous(draft.cfg!.frameworkVersion)
                                ? "Several source builds are registered here, so a bare “source” is ambiguous — pick the one you mean above."
                                : "No matching source build is registered on this machine — add one under Frameworks, or pick a release."}
                            </p>
                          }
                        >
                          {(fw) => (
                            <>
                              <p class="field-hint">
                                Re-read from <code>{fw().path}</code> on every build, so your
                                latest <code>cmake --install</code> is what this compiles against.
                              </p>
                              <Show
                                when={fw().sourceDir}
                                fallback={
                                  <p class="field-hint field-bad">
                                    Its source tree is unknown, so a crash inside the framework
                                    won't open any code. Set it under{" "}
                                    <strong>Frameworks → Locate source</strong>.
                                  </p>
                                }
                              >
                                <p class="field-hint">
                                  Debugging steps into <code>{fw().sourceDir}</code>
                                  {fw().buildType && !carriesDebugInfo(fw().buildType)
                                    ? ` — but it was built ${fw().buildType}, which carries no debug info.`
                                    : "."}
                                </p>
                              </Show>
                            </>
                          )}
                        </Show>
                      </Show>
                    </section>

                    <section class="panel">
                      <h2 class="panel-title">Rendering</h2>
                      <div class="field-grid">
                        {/* A Job runs headless — it has no window, so width/height/flags do not
                            apply and the Hub does not pass them. Only the API is meaningful. */}
                        {/* Drag either field sideways to size the window, or click and type.
                            See `scrubNumber`. */}
                        <Show when={draft.cfg!.kind === "Scene"}>
                          <For
                            each={
                              [
                                ["width", "Width"],
                                ["height", "Height"],
                              ] as const
                            }
                          >
                            {([key, label]) => (
                              <label class="field">
                                <span class="field-label">{label}</span>
                                <input
                                  class="input"
                                  type="number"
                                  min={SIZE_MIN}
                                  max={SIZE_MAX}
                                  title="Drag to resize, or type. Hold Shift while dragging for fine steps."
                                  value={draft.cfg!.rendering.window[key]}
                                  onPointerDown={scrubNumber(
                                    () => draft.cfg!.rendering.window[key],
                                    (v) => setDraft("cfg", "rendering", "window", key, v),
                                  )}
                                  onInput={(e) =>
                                    setDraft("cfg", "rendering", "window", key, +e.currentTarget.value)
                                  }
                                  // Clamped on commit rather than on every keystroke, so typing
                                  // "1280" isn't fought character by character.
                                  onChange={(e) =>
                                    setDraft(
                                      "cfg",
                                      "rendering",
                                      "window",
                                      key,
                                      clampSize(+e.currentTarget.value),
                                    )
                                  }
                                />
                              </label>
                            )}
                          </For>
                        </Show>
                        <label class="field">
                          <span class="field-label">Graphics API</span>
                          <Select
                            value={draft.cfg!.rendering.api}
                            options={[
                              { value: "Vulkan", label: "Vulkan" },
                              { value: "OpenGL", label: "OpenGL" },
                            ]}
                            onChange={(v) =>
                              setDraft("cfg", "rendering", "api", v as "Vulkan" | "OpenGL")
                            }
                          />
                        </label>
                        {/* A Scene's windowing system on Linux. Ignored on Windows/macOS, so only
                            shown there; a Job has no window. `auto` lets the runtime (GLFW)
                            choose. Note OpenGL always runs on X11/XWayland — a Wayland choice
                            with OpenGL is ignored by the runtime. */}
                        <Show when={isLinux && draft.cfg!.kind === "Scene"}>
                          <label class="field">
                            <span class="field-label">Windowing (Linux)</span>
                            <Select
                              value={draft.cfg!.rendering.platform ?? "auto"}
                              options={[
                                { value: "auto", label: "Auto (GLFW default)" },
                                { value: "wayland", label: "Wayland" },
                                { value: "x11", label: "X11" },
                              ]}
                              onChange={(v) =>
                                setDraft(
                                  "cfg",
                                  "rendering",
                                  "platform",
                                  v as "auto" | "x11" | "wayland",
                                )
                              }
                            />
                          </label>
                        </Show>
                      </div>

                      <Show
                        when={draft.cfg!.kind === "Scene"}
                        fallback={
                          <p class="field-hint">
                            {draft.cfg!.kind === "Job" ? (
                              <>
                                This is a <strong>Job</strong> — it runs headless on a device-only
                                context, so there are no window settings.
                              </>
                            ) : (
                              <>
                                This is a <strong>Module</strong> — it is loaded by projects that
                                list it, and never opens a window of its own.
                              </>
                            )}
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
                    </section>

                    <section class="panel">
                      <h2 class="panel-title">Content</h2>
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
                                  {/* Order is the search order, so moving an entry up is a real
                                      setting. */}
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
                              class="btn btn-ghost btn-small self-start"
                              onClick={() => setDraft("cfg", "paths", key, (dirs) => [...dirs, ""])}
                            >
                              + Add folder
                            </button>
                          </div>
                        )}
                      </For>
                      <p class="field-hint">
                        Relative to the project root, searched in order. The runtime resolves
                        relative texture, model and shader paths against these — a scene can just
                        ask for <code>textures/wood.png</code>. The engine's own content is
                        searched last, so a project can shadow a built-in asset by name without
                        losing the rest.
                      </p>
                    </section>

                    {/* The modules list is what the runtime loads when *running* this project. A
                        module project is never run — its dependencies are declared in code — so
                        offering the editor there would only invite a key the runtime never reads. */}
                    <Show when={draft.cfg!.kind !== "Module"}>
                      <section class="panel">
                        <h2 class="panel-title">Modules</h2>
                        {/* A checklist of what this machine actually has, not a text box: a typed
                            name is only discovered to be wrong at launch, while everything here
                            is known to resolve. Anything already in the file that is *not*
                            installed still shows — see unregisteredModules below — so opening a
                            project never silently drops it. */}
                        <Show
                          when={(availableModules() ?? []).length > 0}
                          fallback={
                            <p class="field-hint">
                              No modules available yet. Create one with{" "}
                              <strong>New Project → Module</strong>, or download one from a
                              collection.
                            </p>
                          }
                        >
                          <div class="module-picker">
                            <For each={availableModules()}>
                              {(m) => (
                                <label class="pick-row" classList={{ selected: hasModule(m.id) }}>
                                  <input
                                    type="checkbox"
                                    checked={hasModule(m.id)}
                                    onChange={() => toggleModule(m.id)}
                                  />
                                  <span class="pick-meta">
                                    <span class="pick-name">{m.name}</span>
                                    <span class="pick-sub">
                                      {m.path ?? "ships with the framework"}
                                    </span>
                                  </span>
                                  {/* Which ones a debugger can step into is exactly the
                                      difference between the two sources here, so say it. */}
                                  <Show when={m.hasSource}>
                                    <span class="pick-tag" title="Its source is on this machine, so debugging steps into it">
                                      source
                                    </span>
                                  </Show>
                                  <span class="pick-tag">
                                    {m.source === "project" ? "your project" : "framework"}
                                  </span>
                                </label>
                              )}
                            </For>
                          </div>
                        </Show>

                        {/* Entries the file names that this machine cannot offer — a module built
                            on another machine, or a path. Kept, listed and removable, because
                            dropping a setting the user cannot see would be the worst behaviour. */}
                        <Show when={unregisteredModules().length > 0}>
                          <p class="field-hint">Listed in koral.json but not registered here:</p>
                          <For each={unregisteredModules()}>
                            {(entry) => (
                              <div class="dir-row">
                                <input class="input" value={entry} disabled />
                                <button
                                  type="button"
                                  class="btn btn-ghost btn-icon"
                                  title="Remove"
                                  onClick={() => toggleModule(entry)}
                                >
                                  ✕
                                </button>
                              </div>
                            )}
                          </For>
                        </Show>
                        <p class="field-hint">
                          Optional engine features the runtime loads for this project. Your own
                          module projects are built and copied in automatically when this project
                          builds, and a debugger steps straight into their code. A module that
                          another module depends on must be ticked too.
                        </p>
                      </section>
                    </Show>

                    {/* Sticky, and only present while there is something to save — a permanent
                        bar would make an unchanged project look unsaved. */}
                    <Show when={settingsDirty()}>
                      <div class="save-bar">
                        <span class="save-note">Unsaved changes to koral.json</span>
                        <button type="button" class="btn btn-ghost" onClick={revertSettings}>
                          Revert
                        </button>
                        <button type="submit" class="btn btn-primary" disabled={busy()}>
                          {busy() ? "Saving…" : "Save"}
                        </button>
                      </div>
                    </Show>
                  </form>
                </Show>
              </>
            )}
          </Show>

          {/* --- A collection: what it holds, and what can be done to it --- */}
          <Show when={selectedCollection()}>
            {(c) => (
              <>
                <header class="detail-head">
                  <span class="detail-swatch detail-swatch-collection">▤</span>
                  <div class="detail-title-block">
                    <h1 class="detail-title">{c().title}</h1>
                    <p class="detail-path" title={c().subtitle}>
                      {c().subtitle}
                    </p>
                  </div>
                  <div class="detail-actions">
                    <Show
                      when={c().authored}
                      fallback={
                        <button
                          class="btn btn-ghost"
                          title="Stop following this collection (downloaded projects are kept)"
                          onClick={() => removeCollection(c().key)}
                        >
                          Remove
                        </button>
                      }
                    >
                      {(a) => (
                        <>
                          <button class="btn btn-ghost" onClick={() => openAddUrl(a())}>
                            + Add by URL
                          </button>
                          <button
                            class="btn btn-primary"
                            title={
                              a().git?.remote ? `Push updates to ${a().git!.remote}` : "Publish to GitHub"
                            }
                            onClick={() => openPublishCollection(a())}
                          >
                            {a().git?.remote ? "Publish updates" : "Publish"}
                          </button>
                          <button
                            class="btn btn-ghost btn-icon"
                            title="More actions"
                            onClick={(e) => openContextMenu(e, { collection: c() })}
                          >
                            ⋮
                          </button>
                        </>
                      )}
                    </Show>
                  </div>
                </header>

                <div class="badge-row">
                  <span class="project-kind">{c().contents}</span>
                  <span class="project-fw">
                    {c().labs.length} {entryWord(c().contents)}
                    {c().labs.length === 1 ? "" : "s"}
                  </span>
                  <Show when={c().authored?.git}>
                    <span
                      class="project-git"
                      title={
                        (c().authored!.git!.remote ?? "local git repository") +
                        (c().authored!.git!.dirty ? " · uncommitted changes" : "")
                      }
                    >
                      <span class="git-branch-name">{c().authored!.git!.branch ?? "detached"}</span>
                      <Show when={c().authored!.git!.dirty}>
                        <span class="git-dot">●</span>
                      </Show>
                    </span>
                  </Show>
                  <Show when={!c().authored}>
                    <span class="row-kind">shared with you</span>
                  </Show>
                </div>

                <Show when={c().subscribed?.description || c().authored?.description}>
                  <p class="detail-desc">
                    {c().subscribed?.description || c().authored?.description}
                  </p>
                </Show>

                <Show when={c().error}>
                  <p class="error">Couldn't load this collection: {c().error}</p>
                </Show>

                <div class="detail-sections">
                <section class="panel">
                  <h2 class="panel-title">Contents</h2>
                  <Show
                    when={c().labs.length > 0}
                    fallback={
                      <p class="field-hint">
                        {c().authored
                          ? `No ${entryWord(c().contents)}s yet — right-click a project to add it, or use “+ Add by URL”.`
                          : `This collection has no ${entryWord(c().contents)}s yet.`}
                      </p>
                    }
                  >
                    <ul class="member-list">
                      <For each={c().labs}>
                        {(lab, i) => (
                          <li class="member-row">
                            <span class="member-index">{i() + 1}</span>
                            <span class="member-meta">
                              <span class="member-name">
                                {lab.name}
                                <Show when={lab.kind === "Module" && c().contents === "mixed"}>
                                  <span class="lab-badge lab-badge-module">module</span>
                                </Show>
                                <Show when={labPath(lab)}>
                                  <span class="lab-badge">on disk</span>
                                </Show>
                              </span>
                              <Show when={lab.description}>
                                <span class="member-sub">{lab.description}</span>
                              </Show>
                              <span class="member-sub" title={lab.url}>
                                {repoLabel(lab.url)}
                              </span>
                            </span>
                            <span class="member-actions">
                              <Show
                                when={labPath(lab)}
                                fallback={
                                  <button
                                    class="btn btn-ghost btn-small"
                                    disabled={downloadingLab() !== null}
                                    onClick={() => downloadLab(lab.url)}
                                  >
                                    {downloadingLab() === lab.url ? "Downloading…" : "Download"}
                                  </button>
                                }
                              >
                                <button
                                  class="btn btn-ghost btn-small"
                                  onClick={() => select({ kind: "project", path: labPath(lab)! })}
                                >
                                  Open
                                </button>
                              </Show>
                              <Show when={c().authored}>
                                {(a) => (
                                  <>
                                    <button
                                      class="btn btn-icon btn-move"
                                      title="Move up"
                                      disabled={i() === 0 || collectionBusy() === a().path}
                                      onClick={() => reorderLab(a(), lab.url, true)}
                                    >
                                      ↑
                                    </button>
                                    <button
                                      class="btn btn-icon btn-move"
                                      title="Move down"
                                      disabled={
                                        i() === c().labs.length - 1 || collectionBusy() === a().path
                                      }
                                      onClick={() => reorderLab(a(), lab.url, false)}
                                    >
                                      ↓
                                    </button>
                                    <button
                                      class="btn btn-icon btn-danger"
                                      title="Remove from collection"
                                      disabled={collectionBusy() === a().path}
                                      onClick={() => removeLab(a(), lab.url)}
                                    >
                                      ✕
                                    </button>
                                  </>
                                )}
                              </Show>
                            </span>
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                </section>

                <Show when={c().authored}>
                  <p class="field-hint">
                    A collection is a git repository of submodules. Publish it and share the URL —
                    anyone can add it here and download each entry as their own project.
                  </p>
                  {/* Listed once, but the subscription is real and has to stay cancellable —
                      otherwise merging the two rows would strand it with no way to remove it. */}
                  <Show when={subscriptionFor(c().key)}>
                    {(sub) => (
                      <p class="field-hint">
                        You're also following this collection at <code>{sub().url}</code>, so it
                        is shown once, here.{" "}
                        <button
                          type="button"
                          class="link-button"
                          onClick={() => removeCollection(sub().url)}
                        >
                          Stop following
                        </button>
                        .
                      </p>
                    )}
                  </Show>
                </Show>
                </div>
              </>
            )}
          </Show>

          {/* --- A collection entry that isn't here yet --- */}
          <Show when={selectedLab()}>
            {(picked) => (
              <>
                <header class="detail-head">
                  <span class="detail-swatch detail-swatch-ghost">↓</span>
                  <div class="detail-title-block">
                    <h1 class="detail-title">{picked().lab.name}</h1>
                    <p class="detail-path" title={picked().lab.url}>
                      {repoLabel(picked().lab.url)}
                    </p>
                  </div>
                  <div class="detail-actions">
                    <button
                      class="btn btn-primary"
                      disabled={downloadingLab() !== null}
                      onClick={() => downloadLab(picked().lab.url)}
                    >
                      {downloadingLab() === picked().lab.url ? "Downloading…" : "Download"}
                    </button>
                  </div>
                </header>

                <div class="badge-row">
                  <Show when={picked().lab.kind}>
                    <span
                      class="project-kind"
                    >
                      {picked().lab.kind}
                    </span>
                  </Show>
                  <span class="project-fw">from {picked().collection.title}</span>
                </div>

                <Show when={picked().lab.description}>
                  <p class="detail-desc">{picked().lab.description}</p>
                </Show>

                <div class="detail-sections">
                  <p class="field-hint">
                    Downloading clones it into your projects folder as a copy of your own — the
                    upstream history is dropped, so nothing you change here can be overwritten by
                    the author later. It then appears here as an ordinary project.
                  </p>
                </div>
              </>
            )}
          </Show>

          <Show when={!selectedProject() && !selectedCollection() && !selectedLab()}>
            <div class="empty">
              <p class="muted">Nothing selected.</p>
              <p class="muted-sm">
                Pick a project on the left, or create one to get started.
              </p>
            </div>
          </Show>
        </section>
      </div>

      {/* --- Frameworks --- */}
      <Show when={showFrameworks()}>
        <div class="modal-scrim" onClick={() => setShowFrameworks(false)}>
          <div class="modal modal-wide modal-tall" onClick={(e) => e.stopPropagation()}>
            <h2 class="modal-title">Frameworks</h2>
            <div class="modal-scroll">
              <Show when={error()}>
                <p class="error">{error()}</p>
              </Show>

              {/* Builds from source come first: someone who has registered one is working on the
                  framework itself, and it is what their projects resolve to. Listed even when GitHub
                  is unreachable, since nothing here needs the network. */}
              <div class="fw-local-head">
                <span class="fw-local-title">Built from source</span>
                <button class="btn btn-ghost btn-small" onClick={openAddLocal}>
                  + Add source build
                </button>
              </div>
              <Show
                when={sourceBuilds().length > 0}
                fallback={
                  <p class="field-hint">
                    Point the Hub at a framework you built yourself — the directory you passed to{" "}
                    <code>cmake --install --prefix</code> — to build projects against a version that
                    was never released, and to debug straight into engine code.
                  </p>
                }
              >
                <ul class="fw-list">
                  <For each={sourceBuilds()}>
                    {(fw) => (
                      <li class="fw-card">
                        <span class="fw-meta">
                          <span class="fw-version">
                            {fw.name}
                            <span class="fw-tag">source</span>
                            {/* Debug info is what makes a crash land on a line of framework code, and
                                it is a property of how they built it — so it is stated, not implied. */}
                            <Show when={fw.buildType}>
                              <span
                                class="fw-tag"
                                classList={{ "fw-tag-draft": !carriesDebugInfo(fw.buildType) }}
                                title={buildTypeHint(fw.buildType!)}
                              >
                                {fw.buildType}
                              </span>
                            </Show>
                          </span>
                          <span class="fw-sub" title={fw.path}>
                            {fw.path}
                          </span>
                          <span class="fw-sub" title={fw.sourceDir}>
                            {fw.sourceDir ? `source: ${fw.sourceDir}` : "source tree not found"}
                          </span>
                        </span>
                        <span class="fw-pin" title="What a project's Framework setting has to say to use this build">
                          {sourceBuilds().length === 1 ? "source" : `source:${fw.name}`}
                        </span>
                        <button
                          class="btn btn-ghost btn-small"
                          title="Choose the source tree a debugger should read framework code from"
                          onClick={() => locateSource(fw)}
                        >
                          {fw.sourceDir ? "Change source" : "Locate source"}
                        </button>
                        <button
                          class="btn btn-ghost btn-small"
                          title="Forget this registration. The directory itself is left alone."
                          onClick={() => removeSourceBuild(fw.name)}
                        >
                          Remove
                        </button>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>

              <div class="fw-local-head">
                <span class="fw-local-title">Releases</span>
              </div>

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
                            installed()?.find((i) => !i.local && i.version === fw.version);
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
            </div>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setShowFrameworks(false)}>
                Done
              </button>
            </div>
          </div>
        </div>
      </Show>

      {/* --- Right-click menu --- */}
      <Show when={contextMenu()}>
        {(menu) => (
          <>
            <div
              class="menu-backdrop"
              onClick={() => setContextMenu(null)}
              onContextMenu={(e) => {
                e.preventDefault();
                setContextMenu(null);
              }}
            />
            <div
              class="card-menu context-menu"
              ref={(el) => onMount(() => fitContextMenu(el))}
              style={{ left: `${menu().x}px`, top: `${menu().y}px` }}
            >
              <Show when={menu().project}>
                {(p) => (
                  <>
                    <Show when={p().git}>
                      <div class="menu-info">
                        <span class="menu-info-branch">
                          {p().git!.branch ?? "detached"}
                          <Show when={p().git!.dirty}>
                            <span class="git-dot"> ●</span>
                          </Show>
                        </span>
                        <span class="menu-info-remote">
                          {p().git!.remote ?? "local git repository"}
                        </span>
                      </div>
                    </Show>
                    <Show when={p().kind !== "Module"}>
                      <button
                        type="button"
                        class="menu-item"
                        disabled={isRunning(p().path)}
                        onClick={() => runProject(p().path)}
                      >
                        Build &amp; Run
                      </button>
                    </Show>
                    <button
                      type="button"
                      class="menu-item"
                      disabled={isRunning(p().path)}
                      onClick={() => buildProject(p().path)}
                    >
                      Build
                    </button>
                    <Show when={defaultIde()}>
                      <button type="button" class="menu-item" onClick={() => openInIde(p().path)}>
                        Open in {defaultIde()!.name}
                      </button>
                    </Show>
                    <hr class="menu-divider" />
                    {/* The only way to make a collection: from something to put in it. */}
                    <button
                      type="button"
                      class="menu-item"
                      onClick={() => openCreateCollection(p())}
                    >
                      New collection…
                    </button>
                    <button
                      type="button"
                      class="menu-item"
                      disabled={eligibleCollections(p()).length === 0}
                      title={
                        eligibleCollections(p()).length === 0
                          ? "No collection of yours accepts this kind of project yet"
                          : undefined
                      }
                      onClick={() => openAddToCollection(p())}
                    >
                      Add to collection…
                    </button>
                    <hr class="menu-divider" />
                    <button
                      type="button"
                      class="menu-item"
                      onClick={() => openPublishProject(p())}
                    >
                      {p().git?.remote && ownRemote(p().git!.remote) ? "Update on Git" : "Save to Git"}
                    </button>
                    <button
                      type="button"
                      class="menu-item menu-danger"
                      onClick={() => askRemove(p())}
                    >
                      Remove project
                    </button>
                  </>
                )}
              </Show>

              <Show when={menu().collection}>
                {(c) => (
                  <Show
                    when={c().authored}
                    fallback={
                      <button
                        type="button"
                        class="menu-item menu-danger"
                        onClick={() => {
                          setContextMenu(null);
                          removeCollection(c().key);
                        }}
                      >
                        Stop following
                      </button>
                    }
                  >
                    {(a) => (
                      <>
                        <button
                          type="button"
                          class="menu-item"
                          onClick={() => {
                            setContextMenu(null);
                            openAddUrl(a());
                          }}
                        >
                          Add entry by URL…
                        </button>
                        <button
                          type="button"
                          class="menu-item"
                          onClick={() => openPublishCollection(a())}
                        >
                          {a().git?.remote ? "Publish updates" : "Publish…"}
                        </button>
                        <hr class="menu-divider" />
                        <button
                          type="button"
                          class="menu-item menu-danger"
                          onClick={() => askRemoveCollection(a())}
                        >
                          Remove collection
                        </button>
                      </>
                    )}
                  </Show>
                )}
              </Show>
            </div>
          </>
        )}
      </Show>

      {/* --- Unsaved settings, when the selection is about to move --- */}
      <Show when={pendingSelection()}>
        <div class="modal-scrim">
          <div class="modal" onClick={(e) => e.stopPropagation()}>
            <h2 class="modal-title">Save changes to {draft.cfg?.name}?</h2>
            <p class="field-hint">
              Its <code>koral.json</code> has edits that haven't been written yet.
            </p>
            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setPendingSelection(null)}>
                Keep editing
              </button>
              <button
                type="button"
                class="btn btn-ghost"
                onClick={() => {
                  const next = pendingSelection()!;
                  setPendingSelection(null);
                  applySelection(next);
                }}
              >
                Discard
              </button>
              <button
                type="button"
                class="btn btn-primary"
                disabled={busy()}
                onClick={async () => {
                  if (!(await saveSettings())) return;
                  const next = pendingSelection()!;
                  setPendingSelection(null);
                  applySelection(next);
                }}
              >
                {busy() ? "Saving…" : "Save"}
              </button>
            </div>
          </div>
        </div>
      </Show>

      <Show when={showAddLocal()}>
        <div class="modal-scrim" onClick={() => setAddingLocal(false)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitAddLocal}>
            <h2 class="modal-title">Add a build from source</h2>
            <p class="field-hint">
              Registers a framework you built yourself, so projects can target a version that has
              never been released. It gets no version number — a source tree changes under you —
              so projects target it as <code>source</code> and resolve it fresh on every build.
              Nothing is copied: re-running <code>cmake --install</code> is picked up next build.
            </p>

            <label class="field">
              <span class="field-label">Install prefix</span>
              <span class="field-row">
                <input
                  class="input"
                  value={localPath()}
                  placeholder="/path/to/koral-install"
                  onInput={(e) => setLocalPath(e.currentTarget.value)}
                  onChange={(e) => probeLocalPath(e.currentTarget.value)}
                />
                <button type="button" class="btn btn-ghost" onClick={browseLocalFramework}>
                  Browse…
                </button>
              </span>
            </label>
            <p class="field-hint">
              The directory you passed to <code>cmake --install --prefix</code> — it holds{" "}
              <code>bin/</code>, <code>include/</code> and <code>lib/</code>.
            </p>

            {/* What the Hub found, so the user can see whether debugging will work before they
                commit — rather than discovering it at the first crash. */}
            <Show when={detected()}>
              {(d) => (
                <Show
                  when={d().sourcePath}
                  fallback={
                    <p class="field-hint field-bad">
                      Couldn't find the build this prefix came from, so the source tree is unknown.
                      Point at it below, or a crash inside the framework won't open any code.
                    </p>
                  }
                >
                  <p class="field-hint">
                    Built from <code>{d().sourcePath}</code>
                    {d().buildType ? ` (${d().buildType})` : ""}
                    {d().buildType && d().buildType !== "Debug"
                      ? " — a Release build carries no debug info, so debugging cannot step into it."
                      : "."}
                  </p>
                </Show>
              )}
            </Show>

            <label class="field">
              <span class="field-label">Source tree {detected()?.sourcePath ? "(override)" : "(optional)"}</span>
              <span class="field-row">
                <input
                  class="input"
                  value={localSource()}
                  placeholder={detected()?.sourcePath || "/path/to/Koral"}
                  onInput={(e) => setLocalSource(e.currentTarget.value)}
                />
                <button type="button" class="btn btn-ghost" onClick={browseLocalSource}>
                  Browse…
                </button>
              </span>
            </label>
            <p class="field-hint">
              What a debugger reads framework code from, so a crash inside the engine lands on the
              line that failed.
            </p>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setAddingLocal(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canAddLocal()}>
                {busy() ? "Adding…" : "Add"}
              </button>
            </div>
          </form>
        </div>
      </Show>

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
                    ["Module", "Reusable engine feature (cameras, physics…) that projects load at runtime."],
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
              <code>koral-collection.json</code>. It joins the list on the left, and its projects
              refresh each time you open the Hub.
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

      {/* New collection, always started from the project that will be its first entry. */}
      <Show when={showCreateCollection() && collectionSeed()}>
        <div class="modal-scrim" onClick={() => setCreatingCollection(false)}>
          <form class="modal modal-tall" onClick={(e) => e.stopPropagation()} onSubmit={submitCreateCollection}>
            <h2 class="modal-title">New collection from {collectionSeed()!.name}</h2>

            <div class="modal-scroll">
              <p class="field-hint">
                Creates a git repository that gathers projects as submodules, with{" "}
                <strong>{collectionSeed()!.name}</strong> as its first entry. Publish it afterwards
                and share the URL for others to browse.
              </p>

              <span class="field-label">Contents</span>
              <div class="template-picker">
                <For
                  each={
                    [
                      ["projects", "Projects", "Runnable scenes and jobs — a course's labs."],
                      ["modules", "Modules", "Reusable engine features — a module registry."],
                      ["mixed", "Both", "Labs and the modules they use, side by side."],
                    ] as const
                  }
                >
                  {([value, label, blurb]) => (
                    <button
                      type="button"
                      class="template-card"
                      classList={{ "template-active": collectionContents() === value }}
                      disabled={!contentsAccepts(value, collectionSeed()!.kind)}
                      title={
                        contentsAccepts(value, collectionSeed()!.kind)
                          ? undefined
                          : `A ${collectionSeed()!.kind} cannot go in a ${label.toLowerCase()} collection`
                      }
                      onClick={() => setCollectionContents(value)}
                    >
                      <span class="template-name">{label}</span>
                      <span class="template-blurb">{blurb}</span>
                    </button>
                  )}
                </For>
              </div>

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

              {/* A submodule tracks a URL, so a project that has never been pushed is published
                  first — otherwise there is nothing for the collection to point at. */}
              <Show when={seedNeedsPublish(collectionSeed())}>
                <hr class="modal-divider" />
                <Show
                  when={(accounts()?.length ?? 0) > 0}
                  fallback={
                    <p class="field-hint field-bad">
                      {collectionSeed()!.name} isn't published yet, and a collection tracks its
                      entries by URL. Sign in to GitHub first (Settings → Accounts).
                    </p>
                  }
                >
                  <p class="field-hint">
                    {collectionSeed()!.name} isn't published yet — it'll be pushed to a new
                    repository first, and the collection will track that.
                  </p>
                  <label class="field">
                    <span class="field-label">Account</span>
                    <Select
                      value={addProjectHost()}
                      options={accountOptions()}
                      onChange={setAddProjectHost}
                    />
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

              <label class="field">
                <span class="field-label">Entry description (optional)</span>
                <input
                  class="input"
                  value={labDescription()}
                  placeholder="Draw your first triangle."
                  onInput={(e) => setLabDescription(e.currentTarget.value)}
                />
              </label>
            </div>

            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>

            <div class="modal-actions">
              <button type="button" class="btn btn-ghost" onClick={() => setCreatingCollection(false)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canCreateWithSeed()}>
                {busy() ? "Creating…" : "Create"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      {/* Filing a project into a collection that already exists. */}
      <Show when={addToCollection()}>
        <div class="modal-scrim" onClick={() => setAddToCollection(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitAddToCollection}>
            <h2 class="modal-title">Add {addToCollection()!.name} to a collection</h2>
            <p class="field-hint">
              Added as a submodule tracking the project's git repository.
            </p>

            <div class="project-picker">
              <For each={eligibleCollections(addToCollection())}>
                {(c) => (
                  <label class="pick-row" classList={{ selected: targetCollection() === c.path }}>
                    <input
                      type="radio"
                      name="target-collection"
                      checked={targetCollection() === c.path}
                      onChange={() => setTargetCollection(c.path)}
                    />
                    <span class="pick-meta">
                      <span class="pick-name">{c.title}</span>
                      <span class="pick-sub">
                        {c.labCount} {entryWord(c.contents)}
                        {c.labCount === 1 ? "" : "s"} · {c.path}
                      </span>
                    </span>
                  </label>
                )}
              </For>
            </div>

            <Show when={seedNeedsPublish(addToCollection())}>
              <Show
                when={(accounts()?.length ?? 0) > 0}
                fallback={
                  <p class="field-hint field-bad">
                    This project isn't published yet, and a collection tracks its entries by URL.
                    Sign in to GitHub first (Settings → Accounts).
                  </p>
                }
              >
                <p class="field-hint">
                  This project isn't published yet — it'll be pushed to a new repository first.
                </p>
                <label class="field">
                  <span class="field-label">Account</span>
                  <Select
                    value={addProjectHost()}
                    options={accountOptions()}
                    onChange={setAddProjectHost}
                  />
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
              <button type="button" class="btn btn-ghost" onClick={() => setAddToCollection(null)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!canAddToCollection()}>
                {busy() ? "Adding…" : "Add"}
              </button>
            </div>
          </form>
        </div>
      </Show>

      {/* Adding a repository that isn't one of your projects. */}
      <Show when={addUrlTo()}>
        <div class="modal-scrim" onClick={() => setAddUrlTo(null)}>
          <form class="modal" onClick={(e) => e.stopPropagation()} onSubmit={submitAddUrl}>
            <h2 class="modal-title">
              Add {entryWord(addUrlTo()!.contents)} to {addUrlTo()!.title}
            </h2>
            <p class="field-hint">
              Any git repository, added as a submodule. Its folder name comes from the repo.
            </p>

            <label class="field">
              <span class="field-label">Repository URL</span>
              <input
                class="input"
                value={labUrl()}
                placeholder="https://github.com/course/lab01-triangle.git"
                autofocus
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
              <button type="button" class="btn btn-ghost" onClick={() => setAddUrlTo(null)}>
                Cancel
              </button>
              <button type="submit" class="btn btn-primary" disabled={!labUrl().trim() || busy()}>
                {busy() ? "Adding…" : "Add"}
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
                {/* Empty = follow whatever is installed, rather than pinning a choice. */}
                <Select
                  value={prefs.s!.defaultIde}
                  options={[
                    { value: "", label: `Auto (${defaultIde()?.name ?? "none"})` },
                    ...(ides() ?? []).map((ide) => ({ value: ide.id, label: ide.name })),
                  ]}
                  onChange={(v) => setPrefs("s", "defaultIde", v)}
                />
              </Show>
            </label>

            <label class="field">
              <span class="field-label">Framework for new projects</span>
              <Select
                value={prefs.s!.defaultFrameworkVersion}
                options={[
                  {
                    value: "",
                    // Auto is this machine's source build, else the newest release it can find —
                    // which on a fresh machine means one published but not yet installed. Say what
                    // it actually resolved to, or that it found nothing at all.
                    label: defaults()?.frameworkVersion
                      ? `Auto (${frameworkLabel(defaults()!.frameworkVersion)})`
                      : "Auto (nothing installed, and no releases found)",
                  },
                  ...frameworkChoices().map((v) => ({ value: v.value, label: v.label })),
                ]}
                onChange={(v) => setPrefs("s", "defaultFrameworkVersion", v)}
              />
            </label>
            <p class="field-hint">
              Existing projects are unaffected — each one records its own framework in{" "}
              <code>koral.json</code>.
            </p>

            <Show when={isLinux}>
              <label class="field">
                <span class="field-label">Display server (Linux)</span>
                <Select
                  value={prefs.s!.displayBackend}
                  options={[
                    { value: "", label: "Auto (session default)" },
                    { value: "wayland", label: "Wayland" },
                    { value: "x11", label: "X11" },
                  ]}
                  onChange={(v) => setPrefs("s", "displayBackend", v)}
                />
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
                        Others add this collection with:
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
                    <Select
                      value={publishHost()}
                      options={accountOptions()}
                      onChange={setPublishHost}
                    />
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

      <Show when={shownConsole()}>
        {(c) => (
          <section class="console">
            <div class="console-head">
              <div class="console-tabs">
                <Show when={c().build}>
                  <button
                    type="button"
                    class="console-tab"
                    classList={{ active: c().tab === "build" }}
                    onClick={() => setConsoles(c().path, "tab", "build")}
                  >
                    {c().running ? "Building…" : "Build"}
                  </button>
                </Show>
                <Show when={c().run}>
                  <button
                    type="button"
                    class="console-tab"
                    classList={{ active: c().tab === "output" }}
                    onClick={() => setConsoles(c().path, "tab", "output")}
                  >
                    Output
                  </button>
                </Show>
              </div>
              <button
                class="btn btn-icon"
                title="Close this tab"
                onClick={() => {
                  // Clear the active tab and hand focus to the other. If that one is empty too,
                  // this project's console has nothing left to show and the panel disappears — so
                  // closing the last tab closes it rather than leaving an empty shell.
                  const path = c().path;
                  if (c().tab === "build") {
                    setConsoles(path, "build", "");
                    setConsoles(path, "tab", "output");
                  } else {
                    setConsoles(path, "run", "");
                    setConsoles(path, "tab", "build");
                  }
                }}
              >
                ✕
              </button>
            </div>
            <pre class="console-body">
              <AnsiLog text={c().tab === "build" ? c().build : c().run} />
            </pre>
          </section>
        )}
      </Show>
    </div>
  );
}
