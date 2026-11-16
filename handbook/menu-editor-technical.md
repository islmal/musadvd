# Menu editor – technical reference (for AI agents)

Scope: the custom menu editor (`frontend/src/menuEditor.ts`), its Canvas timeline, the layout JSON model shared with Rust, and the Rust pipeline that turns a layout into DVD/Blu-ray menus. Refer to functions by name – line numbers drift. Project rules: [CLAUDE.md](../CLAUDE.md); architecture overview: [STATUS.md](../STATUS.md). Design plan of the timeline: [2026-11-10-disney-fastplay-and-custom-motion-menus.md](../docs/plans/2026-11-10-disney-fastplay-and-custom-motion-menus.md).

State of this document: 13.11.2026, after the timeline overhaul (command undo, Canvas timeline, waveform, external media, layer tracks, intro keyframes, template import).

## 1. File map

| File | Role |
|---|---|
| `frontend/src/layout.ts` | TS model (`CustomLayout`, `CustomMenu`, `LayoutElement`, `ElementKey`, `Action`, …), `draftLayout`, `isButton`, `isMoving`, `referencedTitles`, `remapLayoutTitles`, `newId`. Mirrors `src/layout.rs` 1:1 (camelCase JSON). |
| `frontend/src/menuEditor.ts` | The whole editor, one closure: `mountMenuEditor(opts)`. ~2850 lines, no framework, DOM built with the `el()` helper; the timeline is one `<canvas>`. Exports `interpolateKeyframes`. |
| `frontend/src/editorWindow.ts` | Entry of `editor.html`: waits for `editor-init`, mounts the editor, forwards `editor-apply` to the main window, closes via `requestClose()`. |
| `frontend/src/api.ts` | Tauri wrappers: `checkLayout`, `loadLayout`, `saveLayout`, `importConvertxTemplate`, `textLines`, `imagePreview`, `titleFrame`, `audioWaveform`. |
| `src-tauri/src/menu_editor.rs` | Commands `check_layout`, `text_lines`, `load_layout`, `save_layout`, `import_convertx_template`, `image_preview`. |
| `src-tauri/src/main.rs` | Commands `title_frame` and `audio_waveform` (they need the toolchain). |
| `src/layout.rs` | Rust model, `validate`, `plan_navigation`, `render_custom_page`, `build_custom_menus`, `video_clip`. |
| `src/bd_menu.rs` / `src/bd_template.rs` | Blu-ray menu build; `layout_from_template` (ConvertX template → `CustomLayout`, also used by the editor import). |
| `src/menu_anim.rs` | Intro keyframe interpolation (`lerp`) and per-frame compositing; `dvd_intro_frames`, `dvd_gop`, `dvd_rate`. |
| `src/encoder.rs` | `custom_menu_args` (ffmpeg graph of a moving menu), `waveform_args` / `waveform_peaks`. |
| `src/tools.rs` | `Toolchain::ffmpeg_output` (ffmpeg with stdout piped, used by the waveform). |
| `src/author.rs` | `MenuSpec`, `spumux_xml`, dvdauthor-style XML and native PGC construction (`intro_secs` → 2 cells). |
| `src/dvdvm.rs` | VM command parser (validates `customVm` scripts). |
| `frontend/src/styles.css` | `.ed-*` classes; timeline: `.ed-tl-scroll`, `.ed-tl-sizer`, `.ed-tl-canvas`. |
| `scripts/check-i18n.mjs` | Build gate: locale key parity, literal `t("…")` keys exist, **no umlauts/ß in .ts code**. |
| Tests | `tests/custom_menu_intro.rs` (intro cells, Easter egg mask, external video args), `tests/menu_transitions.rs`, `tests/fastplay_vm.rs`, `tests/dvdvm_sprm.rs`, `tests/dvdauthor_native.rs`, `tests/bluray.rs`; unit tests in `layout.rs` and `encoder.rs`. There is **no frontend test suite**. |

## 2. Data model

JSON is camelCase on both sides. Rust uses `#[serde(tag="type", rename_all="camelCase")]` for unions.

```ts
CustomLayout { version, canvas:{width,height}, highlight:{color,selectColor,opacity},
               animations:boolean, fastPlay?:FastPlayConfig, menus: CustomMenu[] }
CustomMenu   { id, name, role:"title"|"root"|"settings", title?:number|null,   // title is 1-based
               background:{color, image?, video?:{title, startSecs?, path?}, blur, gradientOverlay?},
               durationSecs?:number|null,   // LOOP length (Cell 2), default 20, clamp 1..600 in Rust
               audio: MenuSound,            // none | title{title,startSecs?} | file{path}
               introSecs?:number|null,      // intro length (Cell 1); None/0 = no intro
               elements: LayoutElement[] }  // index 0 = bottom, last = top
LayoutElement = TextElement | ImageElement | VideoElement
               // common: id,x,y,w,h,rotation,opacity, action?, highlight?, group? (editor only),
               //         intro?: ElementKey[], easterEgg?
VideoElement  { type:"video", title, startSecs?, fit, path? }   // path set → external file, title ignored (0)
ElementKey    { t, x, y, w, h, opacity }                         // seconds into the intro, canvas pixels
Action = playTitle | playChapter | menu{menu,transition?} | playAll | setAudio{source} | setSubtitle{source|null} | customVm{script}
```

Rules that matter when editing the model:

- **A button is any element with `action`** (`isButton(e) = !!e.action`). No separate button type.
- **External video:** `path` is an optional field, not a tagged enum, so every saved layout (`{title, startSecs}`) keeps loading; a tag would also collide with the element's own `"type":"video"`. Rust resolves both sources in `video_clip(titles, title, start, path)`: a file starts at `startSecs ?? 0` and has `has_audio: false`; a title uses `clip_of` (default start 10 % of the runtime, max 5 min). `referencedTitles` / `remapLayoutTitles` skip entries with a `path`.
- Coordinates are in **canvas pixels** (default 720×576 PAL, 720×480 NTSC; Blu-ray layouts must be 16:9, e.g. 1280×720 or the 1920×1080 of an imported template). The stage is scaled to the TV aspect by CSS (`layoutStage`), the model never stores display pixels.
- **Total motion time of a menu = `introSecs + durationSecs`**. Never store an end time.
- `introSecs` is GOP-aligned in Rust: `dvd_intro_frames(secs, tv)` rounds to whole GOPs (PAL 15 frames, NTSC 18). The editor pre-snaps with `snapToClosedGop`; Rust has the final say.
- **Intro keys** (`intro`): interpolated linearly, value held before the first and after the last key (`menu_anim::lerp`, mirrored by `interpolateKeyframes`). Two keys with the same `t` are a hard cut (no division by zero). Keys carry no rotation. Rust runs the intro for `max(introSecs, last key t)`.
- Buttons: max 36 on DVD (`MAX_BUTTONS`; 255 on BD), no overlaps, rotation 0, max 3 distinct colour pairs per menu (`MAX_COLOR_PAIRS`), 2-px grid for DVD hitboxes (the AABB tool snaps x0/y0 even, x1/y1 odd).

## 3. Editor architecture (frontend)

`mountMenuEditor(opts: EditorOptions)` creates one big closure. State (all `let` in the closure):

| State | Meaning |
|---|---|
| `L: CustomLayout` | the working layout (deep-cloned from `opts.layout` or `draftLayout`) |
| `menuIdx`, `selIds` | current menu index; selected element ids (ids, never indexes) |
| `history: (string \| Command)[]` | undo stack, max 60: JSON snapshots or `Command { execute, undo }` |
| `dirty`, `confirmClose`, `errors` | unsaved-changes flag, 2-click close confirm, last `checkLayout` result |
| `viewMode: "basic" \| "timeline"` | timeline visibility |
| `playheadTime`, `isPlaying`, `playTimer`, `timelineZoom`, `lastCell1`, `isDrawingAabb` | timeline / draw-tool state |
| `selKey: {id, i} \| null` | selected intro key (element id + key index) |
| `tlHits`, `tlDropY`, `tlWaves` | timeline hit boxes of the last draw, drop indicator, waveform cache |

**Render model.** `renderAll()` = `renderHeader, renderMenus, renderToolbar, renderStage, renderTimelinePane, renderProps, renderStatus, scheduleCheck`. DOM renders rebuild their subtree (`replaceChildren`). The timeline is a canvas redrawn on demand: `requestTimelineDraw()` sets a dirty flag and draws once per `requestAnimationFrame`. Hot paths: `refreshSelection()` (stage+props), `setPlayhead()` (timecode + canvas; the stage re-renders only when the Cell-1/Cell-2 side flips, otherwise `animateStage()` moves keyed elements in place).

**Undo contract (hybrid).**
- Structural changes (add/delete/reorder elements, menus, imports, stage drags): `remember()` pushes `JSON.stringify(L)` **before** the mutation; continuous gestures snapshot at pointer-down and call `pushHistory(before)` on pointer-up only if something changed (`startDrag`, `dragRow`, `moveElement`).
- Field edits of a known object: `beginEdit(resolve, keys, render)` records `keys` at pointer-down and returns `commit()`, which pushes one `Command` holding only those values (or nothing if unchanged). `resolve` is re-run on undo because a snapshot undo replaces `L` with fresh objects – never capture object references in a command. `beginMenuEdit(keys)` is the same for the current menu and switches back to it on undo. Used by marker drags (`introSecs`, `durationSecs`) and keyframe drags/inserts (`intro`).
- Properties panel inputs (`num`/`bind`) call `remember()` once per focus session.
- `undo()` runs `Command.undo()` or restores a snapshot + `renderAll()`.

**Validation loop.** `scheduleCheck()` debounces `checkLayout(L, titles, tv)` → Rust `layout::validate` (same code as the build) → `errors: string[]` (German) → `renderStatus`. `apply()` re-runs it and only calls `opts.onApply(structuredClone(L))` when empty. The editor stays open after apply.

**Previews.** `frameOf(title,start)` / `frameOfVideo(v)` (cached ffmpeg frame via `titleFrame`; files use their own path), `imageOf(path)`, `linesOf(textElement)` (line breaks from Rust so preview == build). All go through `preview()` keyed by kind+key. `videoLabel(v)` names a title or file; `defaultStart(v)` is the start shown when `startSecs` is empty (title: 10 % / max 300 s, file: 0).

**Input.** `startDrag` (element move/resize incl. group & multi-select; routes to a selected intro key, see §5), stage `pointerdown` (marquee, or AABB draw when `isDrawingAabb`), global `onKey` (capture phase; ignores inputs/selects; timeline keys are handled before the `sel.length===0` early return).

**Toolbar.** `+ Text`, `+ Externes Bild`, `+ Projekt-Titel` (video element of a title), `+ Externes Video` (`addExternalVideo` → `addVideo(path)`, title 0, no action), `+ Button-Bereich`, safe-area toggle, button counter. Header: `Entwurf aus Titeln`, `Laden …`, `Aus Vorlage …`, `Speichern …`, `Rückgängig`, view switch, `Schließen`, `Übernehmen`.

**i18n gate.** `scripts/check-i18n.mjs` fails `npm run build` on any literal `ä ö ü Ä Ö Ü ß` in `.ts` code (comments excluded) and on key mismatches between `locales/{de,en,ru}.json` (flat keys). All editor and timeline strings go through `tr("menuEditor.…")`.

## 4. Canvas timeline

All code is inside `mountMenuEditor`, after `renderStage`.

```
DOM:  timelinePane > tlBar (display:contents: header + hint)
                   > tlScroll (overflow:auto) > tlSizer (sized to the content) + tlCanvas (position:sticky)
Constants: TL_NAME_W 150, TL_RULER_H 28, TL_GAP 4, TL_ROW_H 32 (main rows), TL_BTN_H 18 (element rows)
timelineTotal() = max(1, introSecs + durationSecs)      pxPerSec() = 45 * timelineZoom
fitZoom()       = (centerSection.clientWidth - 190) / (timelineTotal()*45), clamped 0.25..10
snapToClosedGop(s) = round(round(s*fps)/gop)*gop / fps  (fps 25|30000/1001, gop 15|18)
```

- The canvas has the viewport size (× `devicePixelRatio`) and sticks inside a sizer as large as the content, so native scrollbars and the mouse wheel work; `drawTimeline()` subtracts `scrollLeft/scrollTop`. A `ResizeObserver` and the scroll listener request redraws.
- **Rows** (top to bottom): Video (thumbnails of the background or first video, intro/loop regions), Ton (waveform or "kein Ton"), the "Ebenen (n)" header (locked 0..T / active T..T+L), then **one row per element in reverse array order – the top row is the frontmost layer**. Buttons draw the yellow active bar with their action label; static elements a grey bar over the whole runtime.
- **Hit-testing:** every draw rebuilds `tlHits: TlHit[]` (`kind: ruler | markerT | markerL | element | keyframe`, `id`, `key`, `tip`); `tlHitAt(ev)` searches it last-to-first, so keyframes (pushed after their row) win.
- **Pointer handlers on the canvas** (all with `setPointerCapture`; the canvas is never rebuilt):
  - ruler: `dragOnRuler` scrubs the playhead (no undo);
  - T/L markers: `dragOnRuler(ev, onMove, ["introSecs"|"durationSecs"])` → one `beginMenuEdit` command; T snaps to closed GOPs (0..60 s), L = `clamp(snap(max(T+1, secs)) - T, 1, 600)`;
  - element row in the name column: select + `dragRow` (4-px threshold, drop line `tlDropY`); drop calls `moveElement(id, target, !after)` – visually below means *before* in the array; one snapshot undo;
  - element row in the track area: select;
  - keyframe diamond: select key (`selKey`), playhead jumps to `k.t`, `dragKeyframe` shifts `t` (0.01 s steps) between the neighbouring keys and inside `0..introSecs`;
  - double-click on a track inside `0..introSecs`: inserts a key at `t` (copy of the nearest key, else of the element), sorted by `t`; one command.
- **Waveform:** `tlWave(audio)` fetches 660 s of peaks once per source (`audioWaveform(path, start, 660)` → `u8[]`, 40 peaks/s) into `tlWaves`, outside the layout; `drawTimeline` draws one bar per screen column (max peak in that column). Titles use their default start; a file starts at 0.
- **Stage coupling:** in timeline mode buttons with `playheadTime < introSecs` get `hli-inactive` (dimmed, still editable). `boxAt(e)` returns the interpolated key state while the playhead is in the intro; `renderElement` uses it and `animateStage()` updates keyed nodes during playback/scrub.

## 5. Intro keyframes in the editor

- Data: `element.intro: ElementKey[]`, kept sorted by `t`.
- Selection: `selKey` is set by clicking a diamond and cleared by any other timeline click or a stage click on another element; the selected diamond is filled with the accent colour.
- Properties: when `selKey` matches the only selected element, `renderProps` shows `keyProps`: Zeit (clamped between neighbours), X, Y, W, H, Deckkraft, plus "Zum Element" and "Keyframe löschen".
- Stage: `stageKey(e)` decides at pointer-down whether a stage drag edits a key. Selected key of `e` → that key (the playhead first jumps to `k.t`, so the drag starts from the key's box). Another element's key selected and `e` has a key at the playhead (±5 ms) → selection switches to it. Otherwise `selKey` is cleared and the base box is edited. Key drags move one element (no group/multi-select), keep the snap grid, are **not** clamped to the canvas (slide-ins from outside are allowed), update the panel live and push one snapshot undo.
- Limits: image/video previews are fitted to the base size when they load, so an animated size only stretches them in the preview (the build is correct); static element rows do not show keys' visibility.

## 6. Tauri API used by the editor

| Command | Input → output | Notes |
|---|---|---|
| `check_layout` | `(layout, titles: EditorTitle[], tv)` → `string[]` | `layout::validate` / `validate_bd`; empty = valid |
| `text_lines` | `(element, canvas, tv)` → `string[]` | `layout::element_text_lines`, same wrapping as the build |
| `load_layout` / `save_layout` | path ↔ `CustomLayout` | plain JSON |
| `import_convertx_template` | `(path, titles, lang)` → `{layout, warnings}` | async + `spawn_blocking`; `bd_template::layout_from_template` with `Aspect::Wide`, default chapter settings, evenly spread chapter sample times; German errors |
| `image_preview` | path → data URL | |
| `title_frame` | `(path, atSecs)` → data URL | ffmpeg still; cached by 0.1 s |
| `audio_waveform` | `(path, startSecs, secs)` → `u8[]` | ffmpeg `-ac 1 -ar 2000 -f u8 pipe:1` via `Toolchain::ffmpeg_output`, folded by `waveform_peaks` (50 samples → 1 peak) |

The editor window is separate (`editor.html`). Events: main → `editor-init` (layout, titles, tv, heading, layoutTitlesKey); editor → `editor-ready`, `editor-apply` (validated layout). Dialogs use `@tauri-apps/plugin-dialog` (`open`, `save`, `ask`; capabilities in `src-tauri/capabilities`).

## 7. Rust pipeline (layout → disc)

1. `validate_impl(layout, titles, frame, bd)` – canvas 16:9 for BD, colours `#RRGGBB`, unique menu ids, targets exist, external files exist ("Video nicht gefunden: …", "Hintergrundvideo nicht gefunden: …"), buttons ≤ max / non-overlapping / unrotated, colour pairs ≤ 3, `customVm` scripts parse, PGC command budget. Messages are German and shown verbatim.
2. `plan_navigation_with_offset` – assigns each menu a placement (VMGM n / VTSM title,pgc) and builds button commands. Registers: `g1` play-all, `g2` titleset dispatch, `g3` VMGM router. Bridge PGCs cover jumps a VTSM cannot do itself.
3. `render_custom_page` (rayon, per menu) – static elements into transparent PNG layers, video elements as `MenuLayer::Video { clip: video_clip(..) }`, background video as `MenuBackground::Video`, 4-colour highlight/select PNGs, navigation; with `layout.animations && introSecs > 0` a `menu_anim::Intro` (frames = `dvd_intro_frames(max(introSecs, last key t))`). `total_motion_secs = intro.secs + (moving ? loop : 1.0)`.
4. `encoder::custom_menu_args` – one MPEG-2 encode; every clip is an input `-stream_loop -1 -ss <start> -t <d> -i <path>`; with an intro: forced keyframe at `T`, closed GOP, scene cut off.
5. `author` – with `intro_secs = Some(t)` the menu VOB has **two cells**: Cell 1 `0..t` (no SPU buttons), Cell 2 `t..end` (loops via `jump cell 2;`). Without intro one cell.
6. `build_custom_menus` orders menus (VMGM first, then VTSM per title) and adds the FastPlay page when `fastPlay.enabled`.
7. Blu-ray: `bd_menu.rs` builds IG menus from the same layout; it plays an intro only when `introSecs > 0` **and** some element has keys. `CustomLayout.animations=false` disables intros everywhere.

## 8. Known gaps / backlog

1. Per-button visibility windows are not representable (DVD buttons exist per cell; all buttons share Cell 2).
2. Audio crossfade between intro and loop is not implemented.
3. Keys have no rotation; the editor offers no easing (Rust is linear too).
4. Template import is Blu-ray flavoured: the converter's notes mention Blu-ray even in DVD projects, and chapter sample frames are spread evenly (`shortcut:` in `import_convertx_template`).
5. Animated sizes stretch image/video previews (see §5).
6. No frontend tests; `interpolateKeyframes` was checked with ad-hoc Node asserts. Rust coverage: `tests/custom_menu_intro.rs`, `layout.rs`/`encoder.rs` unit tests.
7. Never run a real burn to test (CLAUDE.md); verify with `cargo test` and VOB/m2ts inspection (`build-dvd --layout <json> --keep-work`).

## 9. Recipes

**Add a numeric menu property.** `src/layout.rs` field with `#[serde(default)]`; `frontend/src/layout.ts` optional field; `menuProps`: `row(tr(…), num(value, v => (m.x = v), {min,max,step}))` (`num` records undo); i18n keys in all three locale files; a Rust round-trip test.

**Add a timeline row.** Push to `rows` in `drawTimeline` (`{label, h, sel, id?, keys?, draw(y,h)}`), draw bars with `region(fromSecs, lenSecs, y, h, fill, edge, label, color)`. Add `TL_*_H` to the sizer height. Interactive rows push a `TlHit` and get a branch in the canvas `pointerdown`.

**Add a draggable canvas handle.** Push a `TlHit` with a new `kind` in `drawTimeline`, handle it in the canvas `pointerdown`, drag with pointer capture and record undo with `beginEdit`/`beginMenuEdit` (or a snapshot via `pushHistory(before)`).

**Add an element action type.** `Action` union in `layout.ts` + Rust enum; `actionLabel` and `actionControls` in the editor; command generation in `plan_navigation_with_offset`; validation in `validate_impl`.

## 10. Verification

```powershell
npm run build                                   # i18n + order gates + tsc + vite (must pass)
cargo test --test custom_menu_intro --test menu_transitions --test fastplay_vm --test dvdvm_sprm
npx tauri build --no-bundle                     # release GUI with embedded frontend (or scripts\build_all.ps1)
target\release\convertx-rust.exe build-dvd --input <video> --layout <layout.json> --output <dir> --keep-work --fast
```

Prefix PowerShell sessions with the PATH fix from CLAUDE.md. Steps 1–11 of the overhaul were each exercised in the running release GUI; external video files were verified with full DVD and Blu-ray CLI builds (start offsets matched by PSNR).
