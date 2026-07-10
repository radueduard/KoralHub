import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// @tauri-apps/cli sets TAURI_DEV_HOST when developing against a physical device.
const host = process.env.TAURI_DEV_HOST;

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [solid()],

  // Tauri expects a fixed port and manages the process, so fail instead of hopping ports.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    // Tauri recompiles on its own; don't let Vite watch the Rust side.
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
