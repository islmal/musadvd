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
| `main.rs` | App setup and the main commands: `probe_file`, `list_templates` (empty list when no ConvertX templates are installed), `template_preview`, `calculate_project` (size plan with the frame-size floor and hardware-encoder reserve, quality rating; see [architecture.md](architecture.md) "Size planning"), `source_frame`, `detect_crop`, `title_frame`, `test_title_quality` (10 s sample + PSNR verdict), `detect_scene_chapters`, `start_conversion` (progress as `convert-event`, result as one `convert-done`), `default_output_dir`, settings (`get_settings`, `save_settings`, which also calls `i18n::reload()`), `get_locale_data`. |
| `burn.rs` | Burn after a conversion: `list_drives`, `disc_needs`, `probe_media`, `start_burn`, `cancel_burn`, `startup_burn`, `open_folder`. Events `burn-progress`, `burn-done`. Goes through `burner::burn_video_ts`. |
| `burn_studio.rs` | Burn Studio commands. One operation at a time (`StudioState`) on its own thread; events `studio-progress`, `studio-graph` (at most 4/s), `studio-log`, `studio-done`. |
| `menu_editor.rs` | `check_layout` (layout vs project titles), `text_lines`, `menu_navigation` (remote links), `load_layout`, `save_layout`, `image_preview`. |
| `preview.rs` | Live player: loopback HTTP server, one ffmpeg child, token per file; `preview_stream` (takes optional `PictureOptions` so the preview matches the encode), `preview_stop`. |

`tauri.conf.json` and `capabilities/` define windows and permissions. Without the
`tauri/custom-protocol` feature the exe loads `http://localhost:5173` (dev server) instead of the
embedded `dist/`.

## Frontend (`frontend/src/`)

| File | Purpose |
|---|---|
| `api.ts` | Typed wrappers of the Tauri commands; mirrors Rust types (`DiscType`, `AudioChoice`, settings). |
| `main.ts` | Main window: start page (Home / New Project), title list with multi-select, drag reorder, merge and "Split automatically" (merged DVD titles over 99 chapters), size estimate and conversion; quality badge per title ("82% · Good" with tips, `quality.ts`); Project settings tab (`renderProjectSettings`, applied and saved on every change, `settingsApplied`); Audio/Subtitles tabs with drag handles, one mixed list for embedded and external subtitles (`discSubs`, `setDiscSubs`), batch reorder by language or slot; Subtitles & Audio menus follow track changes (with Undo); TV-standard mismatch banner; menu aspect (auto / 4:3 / 16:9); 99-title count over merge groups; "does not fit" advice per medium (`discRules.overflowHintKey`); in merged titles each video's first chapter is named after the video in drafted chapter menus (`editorTitles`); Blu-ray footer modes (`bdVideo`, `bdAudio`), Copy/Re-encode badges, strict start gates. |
| `order.ts` | Pure helpers (also run by `scripts/check-order.mjs`): `moveItem`, `sortByOrder`, `mergeBlock`, `reorderByLanguages`, mixed subtitle order (`mixSubs`, `unmixSubs`, `dropExternalKind`), 99-chapter checks (`chapterOverflows`, `autoSplitPoints`). |
| `project.ts` | `.musadvd` project files: format, validation (`parseProject`), dialogs, recovery autosave; the subtitle mix (`subKinds`) and the project settings (`settings`, merged by `withProjectSettings`) are saved per project. |
| `landing.ts`, `recent.ts` | Start page (New Project, Load Project, Burn Studio) and the recent-projects list (name, path, medium, last change). |
| `quality.ts` | Pure quality rating: `rate` (percent to excellent/good/fair/low), bits per pixel, `qualityHint` (tips such as Half D1, SIF or a bigger disc). |
| `discRules.ts` | Pure disc decisions: TV-standard mismatch (`tvMismatch`, `fpsMatches`), automatic menu aspect (`autoMenuAspect`), AC-3 rates as the engine snaps them (`snapAc3Kbps`, `ac3EncodeKbps`), "does not fit" advice (`overflowHintKey`). |
| `edit.ts` | "Trim & picture" tab: trimmer with live player, crop inspector, "Detect automatically" and the batch variant for all or the selected videos (`detectCropBatch` in `main.ts`, also in the title context menu), chapter marks, "Detect Scenes", denoise/EQ sliders, 10 s quality test. |
| `menuEditor.ts` | WYSIWYG menu editor: menu manager, stage with drag/resize, properties, Canvas timeline with intro keyframes, template import, rectangles, preset picker (one undo step), "Test Remote" mode (keyboard/D-pad navigation, menu and title preview; `layout.ts` has a fallback navigation if the backend is unavailable). |
| `layout.ts` | `CustomLayout` types (same JSON as `src/layout.rs`), the first-draft generator `draftLayout` and its presets (`swiss`, `cinema`, `midnight`, each with bundled menu music, `PRESET_SOUNDS`); `cleanTitle` for file-name captions. |
| `editorWindow.ts` | Editor window entry: receives project data, reports applied layouts to the main window. |
| `settings.ts` | Settings UI from one set of field builders (`formKit`): the General settings dialog keeps app-level options (language, processing, GPU, track names, template folder, burning); the disc options (`PROJECT_SETTING_KEYS`) are the main window's Project settings tab (`renderProjectSettings`). |
| `burn.ts` | "Burn to DVD" dialog: drive, disc check, label, speed (shows the stored preference until the disc is read, then "Auto (6x for this disc)"), erase pre-ticked for rewritable discs with data, confirmation before erasing or writing write-once media (closed together with the sheet), auto-start countdown, progress. |
| `burnConfirm.ts` | Pure decisions for the erase pre-tick (`eraseDefault`) and the kind of confirmation (`confirmKind`). |
| `burnStudio.ts` | Burn Studio window: menu bar, mode switch, drive bar, status panel, graph tab. |
| `preflight.ts` | Warnings before a conversion starts, from data the GUI already has (`collectWarnings`, `confirmPreflight`); hard blockers stay on the Convert button, and copy failures that only show while muxing cannot be predicted here. |
| `updateDialog.ts` | Update dialog (`openUpdateDialog`): release notes, Install / Later, download progress and a step log. |
| `fontPicker.ts` | Font family picker over the installed families (loaded once for all pickers), used by the subtitle style and the menu editor. |
| `icons.ts` | Menu-editor icon artwork (`iconSvg`, `iconDataUrl`; `STRETCH_ICONS` fill their box). Mirrors `icon_svg_sized` in `src/icons.rs`; keep both in sync. |
| `imageCrop.ts` | Pure image-element crop math for the menu editor: alpha bounding box (`alphaBounds`), cropped aspect, re-fitted box, crop clamping. |
| `snap.ts` | Pure snapping math for the menu editor: canvas centre, action-safe (5 %) and title-safe (10 %) margins, edges and centres of every box (`snapLines`). |
| `textStyle.ts` | CSS for menu text elements (`textBoxCss`, `textGlyphCss`, `hexToRgba`), shared by the editor stage and previews. |
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
| `ui.ts` | Small shared DOM helpers (`el`, `fmtBytes`, `fmtTime`) and `modal`, whose overlay (`modal-overlay`) sits above every sheet (burn sheet, Burn Studio) so a confirmation is never hidden behind its sheet. |

## Conventions

- UI text is looked up with `t('key')`; **no umlauts in `.ts` files** (`scripts/check-i18n.mjs`
  enforces it). Dialogs reopened in the main window must build their text lazily.
- Frontend types that mirror Rust structs must be updated together with the Rust side.
- `npm run build` runs `check-i18n`, `check-order`, `tsc --noEmit` and `vite build`; `npm test`
  runs the vitest suites of the pure modules (`*.test.ts`).
- Keep decisions in pure modules (`order.ts`, `quality.ts`, `discRules.ts`, `burnConfirm.ts`,
  `project.ts`) with a test next to them; `main.ts` only wires them to the DOM.
- Overlay cards (`.overlay-card`) are one grid column `minmax(0, 1fr)`: a long path must never widen
  the card past its edges.
