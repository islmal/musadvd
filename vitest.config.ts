import { defineConfig } from "vitest/config";

// Pure TypeScript modules only (no DOM, no Tauri runtime): `npm test` from the repo root.
export default defineConfig({
  test: { root: "frontend", environment: "node", include: ["src/**/*.test.ts"] },
});
