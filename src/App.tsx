import { createResource, createSignal, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

// Mirrors the `RecentProject` DTO returned by the Rust commands.
type RecentProject = {
  name: string;
  path: string;
  color: [number, number, number];
  frameworkVersion: string;
};

function rgb([r, g, b]: [number, number, number]): string {
  return `rgb(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)})`;
}

export default function App() {
  const [projects, { refetch }] = createResource<RecentProject[]>(() =>
    invoke("list_recent_projects"),
  );
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // Minimal create flow until a proper name/location dialog exists: drop a uniquely named
  // project into the default location, then refresh the list. Proves the create_project
  // command end to end.
  async function newProject() {
    setBusy(true);
    setError(null);
    try {
      const location = await invoke<string>("default_project_location");
      const name = `MyProject-${Date.now().toString(36)}`;
      await invoke("create_project", { req: { location, name } });
      await refetch();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function removeProject(path: string) {
    await invoke("remove_recent", { path });
    await refetch();
  }

  return (
    <div class="app">
      <header class="titlebar">
        <div class="brand">
          <span class="brand-mark">◆</span>
          <span class="brand-name">Koral&nbsp;Hub</span>
        </div>
        <div class="titlebar-actions">
          <button class="btn btn-ghost">Settings</button>
          <button class="btn btn-primary" disabled={busy()} onClick={newProject}>
            {busy() ? "Creating…" : "New Project"}
          </button>
        </div>
      </header>

      <main class="content">
        <h1 class="section-title">Recent Projects</h1>

        <Show when={error()}>
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
                    <span class="project-fw">koral {p.frameworkVersion}</span>
                    <button class="btn btn-play" title="Build &amp; Run">▶</button>
                    <button
                      class="btn btn-icon"
                      title="Remove from list"
                      onClick={() => removeProject(p.path)}
                    >
                      ✕
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
      </main>
    </div>
  );
}
