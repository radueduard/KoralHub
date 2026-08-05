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
  tauri.conf.json    window + bundle config
  capabilities/      webview permissions
```

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
