import { defineConfig } from "vite";

// The sr25519 slot-signer wasm dep initializes via
// `new URL("data:application/wasm;base64,…", import.meta.url)`. Vite's built-in
// asset-import-meta-url transform then tries to resolve that data: URL as a FILE
// (→ ENAMETOOLONG, and a ~230 KB localhost request that 431s, so the wasm never loads).
// This pre-transform drops the `import.meta.url` base for that one dep, leaving the
// data: URL intact so wasm-compat.js can decode it at runtime.
const fixWasmDataUrl = {
  name: "unstation-wasm-dataurl-fix",
  enforce: "pre",
  transform(code, id) {
    if (id.includes("substrate-slot-sr25519-wasm") && code.includes("import.meta.url")) {
      return { code: code.replace(/,\s*import\.meta\.url\s*\)/g, ")"), map: null };
    }
  },
};

// The web UI lives in `src/` (index.html is the entry); built output goes to
// `dist/`, which Tauri serves (see src-tauri/tauri.conf.json `frontendDist`).
export default defineConfig({
  plugins: [fixWasmDataUrl],
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
  // Don't let Vite's dep pre-bundler touch the sr25519 slot-signer wasm package: it
  // ships its wasm inlined as a `data:application/wasm;base64,…` URL, and esbuild
  // rewrites that into a bogus `.vite/deps/data:…` file path — producing a ~230 KB
  // localhost request that 431s and a wasm that never parses. Excluded, the dep is
  // served as-is so the `data:` URL survives (and wasm-compat.js can decode it).
  optimizeDeps: {
    exclude: ["@novasamatech/substrate-slot-sr25519-wasm"],
  },
  clearScreen: false,
});
