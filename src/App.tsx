import { createResource, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

// Mirrors the `RecentProject` DTO returned by the Rust `list_recent_projects` command.
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
  const [projects] = createResource<RecentProject[]>(() =>
    invoke("list_recent_projects"),
  );

  return (
    <div class="app">
      <header class="titlebar">
        <div class="brand">
          <span class="brand-mark">◆</span>
          <span class="brand-name">Koral&nbsp;Hub</span>
        </div>
        <div class="titlebar-actions">
          <button class="btn btn-ghost">Settings</button>
          <button class="btn btn-primary">New Project</button>
        </div>
      </header>

      <main class="content">
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
                    <span class="project-fw">koral {p.frameworkVersion}</span>
                    <button class="btn btn-play" title="Build &amp; Run">▶</button>
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
