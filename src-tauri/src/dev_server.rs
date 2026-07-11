//! Dev-only: make sure the Vite frontend server (`build.devUrl`, http://localhost:1420) is
//! actually being served before the webview tries to load it.
//!
//! `pnpm tauri dev` starts Vite for us via `beforeDevCommand` and waits for it, so this is a
//! no-op there. But launching the debug binary straight from an IDE — the Run/Debug button on a
//! Cargo target is just `cargo run` — skips that step, so the window would open on a dead
//! `localhost:1420` and show "Could not connect to localhost". Here we notice the server is
//! missing, start it from the project root, and block until it answers so the UI is present the
//! moment the window appears.
//!
//! Compiled only in debug builds; release loads the bundled `frontendDist` and never needs a
//! server.

use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Must match `build.devUrl` in `tauri.conf.json` and `server.port` in `vite.config.ts`.
const DEV_ADDR: &str = "localhost:1420";

/// Holds the Vite process we spawned (if any) and stops it (best effort) when the app exits.
///
/// Killing `pnpm` may leave its `node`/Vite child serving the port — which is fine: the next
/// launch sees the port already up and reuses it rather than starting a second copy.
pub struct DevServer(Option<Child>);

impl Drop for DevServer {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// True if something already answers on the dev port. `localhost` can resolve to both `127.0.0.1`
/// and `::1`; Vite may bind only one, so a hit on either address counts.
fn is_up() -> bool {
    DEV_ADDR
        .to_socket_addrs()
        .map(|addrs| {
            addrs.into_iter().any(|addr| {
                TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
            })
        })
        .unwrap_or(false)
}

/// Ensure the dev server is running, starting it if needed. Returns a guard that stops any server
/// this call started; keep it alive for the lifetime of the app.
pub fn ensure() -> DevServer {
    // Already served (e.g. by `tauri dev`) — leave it be.
    if is_up() {
        return DevServer(None);
    }

    // `package.json` lives one level up from this crate (`src-tauri`). Anchoring to the compile
    // time manifest dir means it works regardless of the IDE's working directory.
    let project_root = match std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        Some(dir) => dir.to_path_buf(),
        None => return DevServer(None),
    };

    // On Windows the pnpm shim is a `.cmd`, which `Command` won't find by the bare name.
    let pnpm = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    let child = match Command::new(pnpm).args(["dev"]).current_dir(&project_root).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "koral-hub: could not auto-start the Vite dev server ({e}). \
                 Run `pnpm tauri dev`, or start `pnpm dev` yourself, then relaunch."
            );
            return DevServer(None);
        }
    };

    // Don't open the window until Vite is answering, or it loads onto a connection error.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if is_up() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    if !is_up() {
        eprintln!("koral-hub: the Vite dev server did not come up within 20s; the window may show a connection error.");
    }

    DevServer(Some(child))
}
