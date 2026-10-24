import { defineConfig } from "vite";

// Frontend sources live in ./frontend, the build goes to ./dist (tauri.conf.json: frontendDist).
export default defineConfig({
  root: "frontend",
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2022",
    // Further pages (paths relative to the repo root, where npm runs vite): the menu editor window
    // (frontend/editor.html) and the Burn Studio window (frontend/burnStudio.html).
    rollupOptions: { input: { main: "frontend/index.html", editor: "frontend/editor.html", studio: "frontend/burnStudio.html" } },
  },
});
