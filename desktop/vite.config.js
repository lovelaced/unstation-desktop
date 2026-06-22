import { defineConfig } from "vite";

// The web UI lives in `src/` (index.html is the entry); built output goes to
// `dist/`, which Tauri serves (see src-tauri/tauri.conf.json `frontendDist`).
export default defineConfig({
  root: "src",
  base: "./",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "esnext",
  },
  server: {
    port: 1420,
    strictPort: true,
  },
  clearScreen: false,
});
