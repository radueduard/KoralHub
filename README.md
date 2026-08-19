# Koral Hub

Standalone desktop app to create, manage, install and run [Koral](https://) framework
projects. Replaces the old in-framework Hub (which was itself a Koral scene, and so could
only run once the framework was already built). Koral Hub is a native app: it installs the
framework, downloads/creates projects, and launches them.

**Design goal:** every project's committed data is platform- and device-independent, so a
git link of a project created on macOS clones and runs directly on Windows/Linux. The Hub
resolves the framework version a project declares to a prebuilt, per-platform SDK
(downloading it if missing), then configures/builds/runs it.

## Stack

- **Tauri v2** shell (Rust core, system webview — small binaries)
- **SolidJS + TypeScript + Vite** frontend
- Rust core owns all OS work: git, downloads, unpack, and driving cmake/vcpkg/the runtime

## Prerequisites (Arch / CachyOS)

```sh
# Tauri system dependencies
sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl \
  appmenu-gtk-module libappindicator-gtk3 librsvg

# Rust
sudo pacman -S rustup && rustup default stable

# Node + pnpm
sudo pacman -S nodejs npm && sudo npm install -g pnpm
```

## Run

```sh
pnpm install          # install frontend deps + the Tauri CLI
pnpm tauri dev        # first run compiles the Rust side — give it a few minutes
```

## Layout

```
src/                 SolidJS frontend
src-tauri/
  src/
    main.rs          desktop entry point
    lib.rs           Tauri builder + command registration
    commands.rs      #[tauri::command] handlers exposed to the UI
    model.rs         portable project schema (koral.json)
    project.rs       project storage, recent index, build profiles
    vcpkg.rs         the Hub's own port tree + the library catalogue
  tauri.conf.json    window + bundle config
  capabilities/      webview permissions
```

## Getting projects in

Three ways, all under **Import** at the bottom of the sidebar:

- **Import from Git** — clones a repository into your projects folder.
- **Import a folder** — lists a project that is already on this machine. Nothing is copied, moved or
  git-initialised; the folder is recorded where it lies, so this is safe to point at work in
  progress. It only has to contain a `koral.json`.
- **Add a collection by URL** — subscribes to a published collection.

## Updating from git

Right-click a project or an authored collection → **Update from Git**. This is deliberately an
**overwrite**: the remote wins outright, so local commits on the branch and edits to tracked files
are discarded, and there is no merge that can stop half way through a conflict. Files the remote
does not carry are left alone. For a collection, every entry it checks out moves to the commit the
refreshed manifest records.

A project downloaded from *someone else's* collection has its history deliberately cut (see
`collection::download_lab`) and so has no remote to update from — save it to your own git first.
A subscribed collection has no checkout at all; its manifest is fetched live, and **Refresh from the
author** re-reads it.

## Build profiles

Every project can be built in `Debug`, `Release`, `RelWithDebInfo` or `MinSizeRel`, chosen from the
picker beside ▶ in the project header. Each has a preset and a build tree of its own
(`cmake-build-<profile>/`), so switching costs a rebuild but never disturbs the one you came from.

The choice is machine-local — which configuration you happen to be working in has no business
travelling inside a project's committed `koral.json`, the same split that keeps `CMakePresets.json`
out of git.

`CMakePresets.json` carries all four pairs regardless of which is active, and both IDEs are given
the whole set: CLion gets a run configuration per profile with the matching CMake profile enabled,
and VS Code gets a build task and a launch configuration per profile. The active one is what each
preselects.

## Libraries (vcpkg)

Most projects need nothing here: the SDK vendors everything its public headers expose (glm, imgui,
spdlog, fmt) and hands it over through `Koral::Koral`. A project that declares no libraries gets no
`vcpkg.json`, no `CMAKE_TOOLCHAIN_FILE`, and never involves vcpkg in its build.

For anything else, **project settings → Libraries → Add library** searches a port catalogue and
writes the choice (with its features) into `koral.json`. There is nothing to install and no
`VCPKG_ROOT` to set: the Hub keeps **one vcpkg checkout per machine** under its data directory,
shallow-cloned in the background on the first run that has no copy, and updated only when you ask it
to — a port tree that moved under a project between two builds is the kind of surprise this is meant
to avoid.

If you already maintain your own vcpkg, set `VCPKG_ROOT` and the Hub defers to it entirely rather
than imposing a second copy.

### The CMake names

Installing a port is only half of using it, so the generated `CMakeLists.txt` also carries the
`find_package` and `target_link_libraries` lines for it. **The port name is not the CMake name**, and
nothing derives one from the other — `nlohmann-json` is found as `nlohmann_json`, `entt` as `EnTT`,
`glfw3` exports a target called plainly `glfw`. So the names are recorded per library in
`koral.json` and shown, editable, under each entry in the Libraries panel.

The Hub fills them in, in descending order of authority:

1. what the project already records — including any correction you have made;
2. the port's own `usage` file, which is upstream's recommendation. Only about a fifth of ports
   ship one, but it covers most of the popular ones;
3. what vcpkg actually installed into one of the project's build trees — not a guess at all, but
   only available once a configure has installed the port;
4. failing all of that, the port name, which the picker marks as **guessed** so it can be checked.

A wrong name fails at `find_package` (or at the link, for a wrong target). Fixing it is one field in
the Libraries panel; a rebuild also picks up (3) on its own once the port is installed.

## Working against a framework you built yourself

The Frameworks tab installs published releases. To build against your own source tree instead,
install the SDK and register the prefix:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Debug -DCMAKE_INSTALL_PREFIX=~/koral-sdk <koral-source-dir>
cmake --build build && cmake --install build
```

Then **Frameworks → Add source build** and give it that prefix. It gets **no version**: a source
tree changes under you, so any number stamped on it would be stale by the next build. Projects
target it by kind instead — set a project's framework to the source build and its `koral.json`
records `"frameworkVersion": "source"` (or `"source:<name>"` when several are registered), which
resolves fresh on every build.

Only the path is remembered — nothing is copied and the prefix is never written to — so re-running
`cmake --install` is picked up by the next build with no re-registration. Removing a source build
forgets the registration; the Hub never deletes a tree it did not create.

### Debugging into framework and module code

A source build is the one framework a debugger can step into, so the Hub wires that up. When you
register a prefix it finds the CMake build it came from (matching on `CMAKE_INSTALL_PREFIX`), and
from that build's cache learns two things: the source tree, and whether it was built `Debug`. Both
are shown on the framework's row — a `Release` build carries no debug info, so no amount of path
configuration will make a crash inside it land on a line of source. If the build directory has
since been cleaned, **Locate source** points at the tree by hand.

Every build of a project on a source framework then regenerates:

- `.koral/debug.gdb` — `directory` entries for the framework's tree and for every module project
  the project loads, so gdb opens the right file even when the recorded build path has moved;
- `.vscode/launch.json` — sources that script, and lists the module build directories under
  `additionalSOLibSearchPath` so their symbols resolve as they are loaded;
- `.vscode/c_cpp_properties.json` — the same trees on the browse path, so stepping into `kor::`
  code lands in an editor that can navigate it.

The last two are machine-local and git-ignored. For CLion or a bare `gdb`, pass the script
yourself: `gdb -x .koral/debug.gdb`. Modules you wrote need none of this — the Hub builds them
from source on this machine already, so their debug info points at code that is right here.

## Icons

`src-tauri/icons/` currently holds placeholder PNGs. Generate the real set (all sizes plus
`.ico`/`.icns`, required for `tauri build`) from a square source image:

```sh
pnpm tauri icon path/to/koral-logo.png
```
