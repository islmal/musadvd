// Entry point of the menu editor window (editor.html): waits for the main window's data,
// mounts the editor and reports applied layouts back.
import { emitTo, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { mountMenuEditor, requestClose, type EditorInit } from "./menuEditor";

const win = getCurrentWindow();
let mounted = false;

const ready = listen<EditorInit>("editor-init", ({ payload }) => {
  if (mounted) return;
  mounted = true;
  mountMenuEditor({
    ...payload,
    onApply: (layout) => void emitTo("main", "editor-apply", layout),
    onClose: () => void win.destroy(),
  });
});

// A close with unapplied changes needs the editor's two-click confirm first.
void win.onCloseRequested((ev) => {
  if (mounted && !requestClose()) ev.preventDefault();
});

// Ask for the data only once the listener is registered.
void ready.then(() => emitTo("main", "editor-ready"));
