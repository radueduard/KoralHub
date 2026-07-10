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

## Icons

`src-tauri/icons/` currently holds placeholder PNGs. Generate the real set (all sizes plus
`.ico`/`.icns`, required for `tauri build`) from a square source image:

```sh
pnpm tauri icon path/to/koral-logo.png
```
