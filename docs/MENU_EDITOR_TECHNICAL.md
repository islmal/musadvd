# Menu editor – technical reference (for AI agents)

Scope: the custom menu editor (`frontend/src/menuEditor.ts`), its timeline view, the layout JSON model shared with Rust, and the Rust pipeline that turns a layout into DVD menu VOBs. Refer to functions by name – line numbers drift. Project rules: [CLAUDE.md](../CLAUDE.md); architecture overview: [STATUS.md](../STATUS.md). Design plan of the timeline: [2026-11-10-disney-fastplay-and-custom-motion-menus.md](plans/2026-11-10-disney-fastplay-and-custom-motion-menus.md).

## 1. File map

| File | Role |
|---|---|
| `frontend/src/layout.ts` | TS model (`CustomLayout`, `CustomMenu`, `LayoutElement`, `Action`, …), `draftLayout`, `isButton`, `isMoving`, `madeForOtherTitles`, `pixelAspectOf`, `newId`. Mirrors `src/layout.rs` 1:1 (camelCase JSON). |
| `frontend/src/menuEditor.ts` | The whole editor, one closure: `mountMenuEditor(opts)`. ~2200 lines, no framework, DOM built with the `el()` helper. |
| `frontend/src/editorWindow.ts` | Entry of `editor.html`: waits for `editor-init`, mounts the editor, forwards `editor-apply` to the main window, closes via `requestClose()`. |
| `frontend/src/api.ts` | Tauri wrappers: `checkLayout`, `loadLayout`, `saveLayout`, `textLines`, `imagePreview`, `titleFrame`. |
| `src-tauri/src/menu_editor.rs` | Tauri commands `check_layout`, `text_lines`, `load_layout`, `save_layout`, `image_preview` (`title_frame` lives in `main.rs`). |
| `src/layout.rs` | Rust model, `validate`, `plan_navigation`, `render_custom_page`, `build_custom_menus`. |
| `src/menu_anim.rs` | Intro keyframe interpolation + per-frame compositing; `dvd_intro_frames`, `dvd_gop`, `dvd_rate`. |
| `src/encoder.rs` | `custom_menu_args` (ffmpeg graph for a moving menu; `-force_key_frames expr:gte(t,T)`, `+cgop` when an intro exists). |
| `src/author.rs` | `MenuSpec`, `spumux_xml`, dvdauthor-style XML + native PGC construction (`intro_secs` → 2 cells). |
| `src/dvdvm.rs` | VM command parser (used to validate `customVm` scripts). |
| `frontend/src/styles.css` | `.ed-*` classes; timeline block starts at `/* Multi-Track Studio Timeline */`. |
| `scripts/check-i18n.mjs` | Build gate: locale key parity, literal `t("…")` keys exist, **no umlauts/ß in .ts code**. |
| Tests | `tests/custom_menu_intro.rs`, `tests/menu_transitions.rs`, `tests/fastplay_vm.rs`, `tests/dvdvm_sprm.rs`, `tests/dvdauthor_native.rs`, `tests/bluray.rs`. There is **no frontend test suite**. |

## 2. Data model

JSON is camelCase on both sides. Rust uses `#[serde(tag="type", rename_all="camelCase")]`.

```ts
CustomLayout { version, canvas:{width,height}, highlight:{color,selectColor,opacity},
               animations:boolean, fastPlay?:FastPlayConfig, menus: CustomMenu[] }
CustomMenu   { id, name, role:"title"|"root"|"settings", title?:number|null,   // title is 1-based
               background:{color,image?,video?:{title,startSecs?},blur,gradientOverlay?},
               durationSecs?:number|null,   // LOOP length (Cell 2), default 20, clamp 1..600 in Rust
               audio: MenuSound,            // none | title{title,startSecs?} | file{path}
               introSecs?:number|null,      // intro length (Cell 1); None/0 = no intro
               elements: LayoutElement[] }  // bottom → top
LayoutElement = TextElement | ImageElement | VideoElement   // common: id,x,y,w,h,rotation,opacity,
               // action?, highlight?, group? (editor only), intro?: ElementKey[], easterEgg?
Action = playTitle | playChapter | menu{menu,transition?} | playAll | setAudio{source} | setSubtitle{source|null} | customVm{script}
```

Rules that matter when editing the model:

- **A button is any element with `action`** (`isButton(e) = !!e.action`). No separate button type.
- Coordinates are in **canvas pixels** (default 720×576 PAL, 720×480 NTSC; BD 16:9). The stage is scaled with `scale(scaleX, scaleY)` to 4:3 (`layoutStage`), so DVD non-square pixels are handled by CSS, not by the model.
- **Total motion time of a menu = `introSecs + durationSecs`** (if `introSecs>0` the loop is `durationSecs`, intro is added in front; Rust: `total_motion_secs = intro.secs + loop_secs`). Never store "end time" in the model.
- `introSecs` is GOP-aligned in Rust: `dvd_intro_frames(secs, tv)` rounds to whole GOPs (PAL 15 frames, NTSC 18). The editor pre-snaps with `snapToClosedGop`; Rust has the final say (`files.intro.secs` can differ slightly from `introSecs`).
- Buttons: max 36 on DVD (`MAX_BUTTONS`; 255 on BD), no overlaps, rotation must be 0, max 3 distinct colour pairs per menu (`MAX_COLOR_PAIRS`), 2-px grid for DVD hitboxes (the AABB tool snaps x0/y0 even, x1/y1 odd).

## 3. Editor architecture (frontend)

`mountMenuEditor(opts: EditorOptions)` creates one big closure. State (all `let` in the closure):

| State | Meaning |
|---|---|
| `L: CustomLayout` | the working layout (deep-cloned from `opts.layout` or `draftLayout`) |
| `menuIdx`, `selIds` | current menu index; selected element ids (ids, never indexes) |
| `history: string[]` | undo stack of `JSON.stringify(L)` snapshots, max 60 |
| `dirty`, `confirmClose`, `errors` | unsaved-changes flag, 2-click close confirm, last `checkLayout` result |
| `viewMode: "basic"\|"timeline"` | timeline visibility |
| `playheadTime`, `isPlaying`, `playTimer`, `timelineZoom`, `lastCell1`, `isDrawingAabb` | timeline / draw-tool state |

**Render model.** `renderAll()` = `renderHeader, renderMenus, renderToolbar, renderStage, renderTimelinePane, renderProps, renderStatus, scheduleCheck`. Every render fully rebuilds its DOM subtree (`replaceChildren`). Partial renders exist for hot paths: `refreshSelection()` (= stage+props), `setPlayhead()` (patches playhead/timecode in place).

**Undo contract (important).** `remember()` pushes the *current* state and sets `dirty`; call it **before** mutating `L`. For continuous gestures (drag) snapshot `JSON.stringify(L)` at pointer-down and push it on pointer-up only if the JSON changed (see `startDrag`, `dragOnRuler`). `undo()` pops and `renderAll()`s. Calling `remember()` after the mutation makes undo a no-op – this was a bug in the first timeline.

**Validation loop.** `scheduleCheck()` debounces `checkLayout(L, titles, tv)` → Rust `layout::validate` (same code as the build) → `errors: string[]` (German) → `renderStatus`. `apply()` re-runs it and only calls `opts.onApply(structuredClone(L))` when empty. The editor stays open after apply.

**Previews.** `frameOf(title,start)` (cached ffmpeg frame via `titleFrame`), `imageOf(path)`, `linesOf(textElement)` (line breaks from Rust so preview == build). All go through `preview()` cache keyed by kind+key.

**Input.** `startDrag` (element move/resize incl. group & multi-select), stage `pointerdown` handler (marquee, or AABB draw when `isDrawingAabb`), global `onKey` (capture phase; ignores inputs/selects). Timeline keys are handled in `onKey` *before* the `sel.length===0` early return.

**Gotcha – i18n gate.** `scripts/check-i18n.mjs` fails `npm run build` on any literal `ä ö ü Ä Ö Ü ß` in `.ts` code (comments excluded). The timeline strings are hard-coded German and written with `ä`-style escapes. Proper fix: move them to `locales/{de,en,ru}.json` and use `tr("menuEditor.…")` (all three files must have the same keys/placeholders).

## 4. Timeline subsystem

All code is inside `mountMenuEditor`, between `renderStage` and `// ---- selection, drag & resize`.

```
timelineTotal()    = max(1, introSecs + durationSecs)
pxPerSec()         = 45 * timelineZoom
fitZoom()          = (centerSection.clientWidth - 190) / (timelineTotal()*45), clamped 0.25..10
snapToClosedGop(s) = round(round(s*fps)/gop)*gop / fps      (fps 25|30000/1001, gop 15|18)
formatTimecode(s)  = HH:MM:SS:FF
stopPlay()/togglePlay()   playback: setInterval 40 ms, real dt via performance.now();
                          at timelineTotal() wraps to introSecs (intro plays once, loop repeats)
setPlayhead(t)     clamps, patches .ed-timeline-timecode and .ed-timeline-playhead,
                   re-renders the stage ONLY when the Cell-1/Cell-2 side flips (lastCell1)
dragOnRuler(ev,onMove)   window-level pointermove/up (element capture breaks because the pane is
                   rebuilt during a drag); records undo on release; renderAll() on release
renderTimelinePane()     builds header, hint, ruler (+ markers), two-column grid of rows
```

DOM produced by `renderTimelinePane`:

```
.ed-timeline-pane (flex column, height 280px)
 ├ .ed-timeline-header   play, timecode, "Intro endet hier", "Kein Intro" (only if T>0), GOP pill, Einpassen, zoom slider
 ├ .ed-timeline-hint     plain-language explanation (changes with T>0)
 └ .ed-tl-body           overflow-y:auto (vertical scroll for many buttons)
    └ .ed-tl-grid (flex)
       ├ .ed-tl-names    fixed 150 px column; one .ed-tl-name per row (+ .ed-tl-ruler-spacer 28 px)
       └ .ed-tl-scroll   overflow-x:auto
          └ .ed-timeline-tracks (width = totalSecs*px+40)
             ├ .ed-timeline-ruler  .ed-tick, .ed-marker-t (blue, introSecs), .ed-marker-l (green, T+L)
             ├ .ed-track.main  Video  (.ed-track-thumb*, .ed-track-intro, .ed-track-loop)
             ├ .ed-track.main  Ton    (.ed-track-audio)
             ├ .ed-track.main  Buttons(.ed-track-buttons-locked 0..T, .ed-track-buttons-active T..T+L)
             ├ .ed-track.btn-row  one per button (click/pointerdown selects the button)
             └ .ed-timeline-playhead (absolute, full height)
```

Row alignment between names and bars relies on identical heights (`.main` 32 px, `.btn-row` 18 px) and identical flex `gap: 4px`. If you change a row height, change it in both `.ed-tl-name.*` and `.ed-track.*`.

Scroll position (`.ed-tl-scroll` scrollLeft, `.ed-tl-body` scrollTop) is saved/restored around every rebuild; any new rebuild path must keep that.

Semantics wired to the stage: in timeline mode a button with `playheadTime < introSecs` gets class `hli-inactive` (dimmed, badge "Subpicture Inaktiv"); it stays editable (an earlier version disabled pointer events, which locked the whole menu at playhead 0).

Markers: T drag sets `m.introSecs = snapToClosedGop(clamp(secs,0,60))`; L drag sets `m.durationSecs = clamp(snap(max(T+1, secs)) - T, 1, 600)`. Both call `renderTimelinePane()+renderStage()` during the drag and `renderAll()` on release.

## 5. Tauri API used by the editor

| Command | Input → output | Notes |
|---|---|---|
| `check_layout` | `(layout, titles: EditorTitle[], tv)` → `string[]` | runs `layout::validate` / `validate_bd`; empty = valid |
| `text_lines` | `(element, canvas, tv)` → `string[]` | `layout::element_text_lines`, same wrapping as the build |
| `load_layout` / `save_layout` | path ↔ `CustomLayout` | plain JSON file |
| `image_preview` | path → data URL | |
| `title_frame` | `(path, atSecs)` → data URL | ffmpeg still; cached in the editor by 0.1 s key |

The editor window is separate (`editor.html`). Events: main → `editor-init` (EditorInit: layout, titles, tv, heading, layoutTitlesKey); editor → `editor-ready`, `editor-apply` (validated layout).

## 6. Rust pipeline (layout → disc)

1. `validate_impl(layout, titles, frame, bd)` – list of rules: canvas 16:9 for BD, colours `#RRGGBB`, unique menu ids, targets exist, buttons ≤ max / non-overlapping / unrotated, colour pairs ≤ 3, `customVm` scripts must pass `dvdvm::parse`, PGC command budget (`MAX_PGC_COMMANDS`). Messages are German and shown verbatim in the status bar.
2. `plan_navigation_with_offset` – assigns each menu a `Placement` (VMGM n / VTSM title,pgc) and builds the button command strings. Registers: `g1` play-all, `g2` titleset dispatch (`1..99` chapter, `100+j` j-th root menu), `g3` VMGM router (`1000+t` play title, `2000+t` enter titleset). Bridge PGCs exist for jumps a VTSM cannot do itself.
3. `render_custom_page` (parallel per menu, rayon) – draws static elements into transparent frame-size PNG layers (text via `menu::text_coverage`, images fitted with the TV pixel aspect), keeps video elements as separate `MenuLayer::Video`, builds 4-colour palette highlight/select PNGs (value 0 transparent), `author::compute_navigation` for up/down/left/right, then:
   - `has_intro = layout.animations && menu.intro_secs > 0`; if so builds `menu_anim::Intro` with `frames = dvd_intro_frames(max(introSecs, last key t))`, `secs`, `ticks`, `sprites`.
   - `moving = intro || video background || video layer || audio`; `total_motion_secs = intro.secs + (moving ? loop : 1.0)`.
   - writes `*_hl.png`, `*_sel.png`, `*_spumux.xml` (`author::spumux_xml(..., intro_secs)`).
4. `encoder::custom_menu_args` – one continuous MPEG-2 encode; with an intro: forced keyframe at `T`, closed GOP, scene-cut detection off (so Cell 2 starts on a clean GOP).
5. `author` – `MenuSpec{ intro_secs, motion, pause_secs, post }`. With `intro_secs = Some(t)` the menu VOB is **two cells**: Cell 1 `0..t` (program, **no SPU buttons**), Cell 2 `t..end` (`still` per `pause_secs`, loops via `post: jump cell 2;` when `motion`). Without intro: one cell, `jump cell 1;`. Both the XML writer and the native `dvd_mux` path implement this (`custom_menu_two_cell_intro_and_closed_gop_structure` test: PAL, 3.0 s → 75 frames).
6. `build_custom_menus` orders menus (VMGM first, then VTSM per title), optional FastPlay page (VMGM menu 1) when `fastPlay.enabled`.

## 7. What the intro really is (read this before extending the timeline)

On DVD, `introSecs` is a **pre-roll animation** whose content is generated by `menu_anim` from each element's `intro: ElementKey[]` keyframes (box + opacity over time, linear interpolation, first/last key hold). It is *not* a separate clip.

Consequences:
- Setting only `introSecs` (what the timeline does today) with no element keyframes yields a static copy of the page for T seconds with the buttons disabled, then the loop. This is valid output but the "intro" does nothing visible by itself.
- **The editor has no UI to create or edit `ElementKey`s** (`intro` exists in the TS type; nothing in `menuEditor.ts` writes it). This is the natural next feature: per-element keyframe tracks in the timeline (drag a key diamond → `{t,x,y,w,h,opacity}`; `t` must be `≤ introSecs`, Rust uses `max(introSecs, last key t)`).
- `bd_menu.rs` (Blu-ray) only plays an intro when `introSecs>0` **and** some element has keys (`menu_has_intro`). DVD does not require keys.
- `CustomLayout.animations=false` disables intros everywhere (build option on BD: `opts.menu_animations`).
- `layout.rs` doc comments say editor layouts' intro is "Blu-ray only"; the DVD code path above does honour it (covered by `tests/custom_menu_intro.rs`). Treat the comment as stale; verify with a real build before relying on it.

## 8. Known gaps / backlog

1. Element intro keyframe editing (see §7). Data model, Rust and encoder already support it.
2. Per-button visibility windows – not representable (DVD buttons exist per cell; all buttons share Cell 2). Would need multiple SPU cells.
3. Audio waveform and the 500 ms crossfade shown in the original plan are **not implemented** (no host-side waveform extraction; the crossfade label was removed).
4. Timeline strings are not localized (§3 gotcha). `en.json`/`ru.json` have no timeline keys.
5. Thumbnails assume the clip starts at `startSecs` for Cell 2 and `startSecs + t` for Cell 1; verify against the encoder's actual clip offsets.
6. No frontend tests; the only automated coverage of the intro path is Rust (`tests/custom_menu_intro.rs`).
7. `menuProps` "Intro-Dauer (T)" input calls `renderAll()` from a *live* `input` handler (via `num`), which rebuilds the props panel on every keystroke and drops input focus; use `{rerender}`-style deferral (render on `change`) when fixing.
8. Never run a real burn to test (see CLAUDE.md); validate via `cargo test --test custom_menu_intro` and ISO/VOB inspection.

## 9. Recipes

**Add a numeric menu property (end to end).**
1. `src/layout.rs` `CustomMenu`: add field with `#[serde(default)]` (+ `skip_serializing_if` if optional); use it in `render_custom_page`/`validate_impl`.
2. `frontend/src/layout.ts` `CustomMenu`: add the optional field; default in `emptyMenu`/`draftLayout` if needed.
3. `menuEditor.ts` `menuProps`: add a `row(label, num(value, v => m.x = v, {min,max,step}))`. `num`/`bind` call `remember()` for you; if the change affects the timeline call `renderAll()` in the callback.
4. i18n: add key to all three locale files (or use the escape-sequence workaround for hard-coded German).
5. Test in Rust; a JSON round-trip test in `layout.rs` unit tests.

**Add a timeline track.** In `renderTimelinePane` use `addRow(label, "main", row => { row.append(region(cls, fromSecs, lenSecs, text)); })`. The helper creates the matching name cell, so alignment is automatic. Use `region()` for time-aligned bars (it multiplies by `px`). Add CSS next to `.ed-track-audio`.

**Add a draggable marker.** Create the element in the ruler, `addEventListener("pointerdown", ev => dragOnRuler(ev, secs => { mutate model; renderTimelinePane(); renderStage(); }))`. Do not use `setPointerCapture`.

**Add an element action type.** `Action` union in `layout.ts` + Rust enum `Action`; `actionLabel` and `actionControls` in the editor; command generation in `plan_navigation_with_offset`; validation in `validate_impl`.

## 10. Verification commands

```powershell
npm run build                                   # i18n gate + tsc + vite (must pass)
npx tsc --noEmit
cargo test --test custom_menu_intro --test menu_transitions --test fastplay_vm --test dvdvm_sprm
cargo build --release -p convertx-gui --features tauri/custom-protocol   # or scripts\build_all.ps1
```

Prefix PowerShell sessions with the PATH fix from CLAUDE.md. There is no automated UI test; the timeline was type-checked and built, **not** exercised in the running GUI by the author of this document.
