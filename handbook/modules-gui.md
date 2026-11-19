# GUI modules (`src-tauri/` and `frontend/`)

The desktop app is Tauri 2 with a vanilla TypeScript frontend (Vite). There is one backend process
and three windows, all started through `frontend/src/boot.ts` (which awaits `initI18n` first).

| Window | HTML | Entry module |
|---|---|---|
| Main (project, edit, convert) | `frontend/index.html` | `main.ts` |
| Menu editor | `frontend/editor.html` | `editorWindow.ts` |
| Burn Studio | `frontend/burnStudio.html` | `burnStudio.ts` |

## Backend (`src-tauri/src/`)

| File | Purpose |
|---|---|
| `main.rs` | App setup and the main commands: `probe_file`, `list_templates` (empty list when no ConvertX templates are installed), `template_preview`, `calculate_project`, `source_frame`, `detect_crop`, `title_frame`, `start_conversion` (progress as `convert-event`, result as one `convert-done`), `default_output_dir`, settings (`get_settings`, `save_settings`, which also calls `i18n::reload()`), `get_locale_data`. |
| `burn.rs` | Burn after a conversion: `list_drives`, `disc_needs`, `probe_media`, `start_burn`, `cancel_burn`, `startup_burn`, `open_folder`. Events `burn-progress`, `burn-done`. Goes through `burner::burn_video_ts`. |
| `burn_studio.rs` | Burn Studio commands. One operation at a time (`StudioState`) on its own thread; events `studio-progress`, `studio-graph` (at most 4/s), `studio-log`, `studio-done`. |
| `menu_editor.rs` | `check_layout` (layout vs project titles), `text_lines`, `load_layout`, `save_layout`, `image_preview`. |
| `preview.rs` | Live player: loopback HTTP server, one ffmpeg child, token per file; `preview_stream`, `preview_stop`. |

`tauri.conf.json` and `capabilities/` define windows and permissions. Without the
`tauri/custom-protocol` feature the exe loads `http://localhost:5173` (dev server) instead of the
embedded `dist/`.

## Frontend (`frontend/src/`)

| File | Purpose |
|---|---|
| `api.ts` | Typed wrappers of the Tauri commands; mirrors Rust types (`DiscType`, `AudioChoice`, settings). |
| `main.ts` | Main window: title list, drag reorder, merge, project settings, size estimate, conversion; Blu-ray footer modes (`bdVideo`, `bdAudio`), Copy/Re-encode badges per title and audio track, strict start gates. |
| `order.ts` | Pure title-list helpers (`moveItem`); also run by `scripts/check-order.mjs`. |
| `edit.ts` | "Schnitt & Bild" tab: trimmer with live player, crop inspector, chapter marks. |
| `menuEditor.ts` | WYSIWYG menu editor: menu manager, stage with drag/resize, properties, Canvas timeline with intro keyframes, template import, rectangles, preset picker (one undo step). |
| `layout.ts` | `CustomLayout` types (same JSON as `src/layout.rs`), the first-draft generator `draftLayout` and its presets (`swiss`, `cinema`, `midnight`). |
| `editorWindow.ts` | Editor window entry: receives project data, reports applied layouts to the main window. |
| `settings.ts` | General settings dialog (navigation tree + pages). |
| `burn.ts` | "Burn to DVD" dialog: drive, disc check, label, speed, auto-start countdown, progress. |
| `burnStudio.ts` | Burn Studio window: menu bar, mode switch, drive bar, status panel, graph tab. |
| `i18n.ts` | `t`, `applyStatic` (`data-i18n*`), `fmtNumber`, runtime catalog loading. |
| `label.ts` | Volume label rules (mirror of `burner::label`). |
| `boot.ts` | Common entry of all windows. |
| `styles.css` | Shared styles. |

### `frontend/src/studio/` (Burn Studio)

| File | Purpose |
|---|---|
| `api.ts` | Command/event wrappers and `BurnStudioSettings` mirror of `src/config.rs`. |
| `views.ts` | The modes: home, write image, write files, read disc, build image, verify, Discovery. |
| `graph.ts` | Canvas graph: speed, main buffer, device buffer, CPU; live or from `.ibg`. |
| `settings.ts` | Seven-tab settings dialog bound to `AppSettings.burn_studio`; unsupported options stay visible but disabled. |
| `tools.ts` | "Werkzeuge" modals: volume label, layer break entry, layout files, drive info, auto speed table. |
| `layerbreak.ts` | Layer break candidates modal. |
| `speeds.ts` | Speed dropdown choices (safe fixed speeds merged with drive-reported ones). |
| `ui.ts` | Small shared DOM helpers (`el`, `fmtBytes`, `fmtTime`). |

## Conventions

- UI text is looked up with `t('key')`; **no umlauts in `.ts` files** (`scripts/check-i18n.mjs`
  enforces it). Dialogs reopened in the main window must build their text lazily.
- Frontend types that mirror Rust structs must be updated together with the Rust side.
- `npm run build` runs `check-i18n`, `check-order`, `tsc --noEmit` and `vite build`.
