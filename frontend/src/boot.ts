// Entry of every window: loads the language first, then the window's module (its top-level code
// already builds translated DOM).
import { getSettings } from "./api";
import { initI18n } from "./i18n";

const entries: Record<string, () => Promise<unknown>> = {
  main: () => import("./main"),
  editor: () => import("./editorWindow"),
  studio: () => import("./burnStudio"),
};

await initI18n(getSettings);
await entries[document.body.dataset.entry ?? "main"]();
