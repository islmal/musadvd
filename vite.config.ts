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
  },
});
