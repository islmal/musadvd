# Title list: drag reorder (pointer events) + merge pre-check

Approved 2026-10-28 (variant: pointer events, NOT HTML5 DnD).

## Goal

Reorder the title cards in `#title-list` by dragging. Keep the native file drop working, keep the
custom-layout stale block, and show merge-group track mismatches in the GUI before the build fails.

## Findings that shape the design

- `src-tauri/tauri.conf.json` has `"dragDropEnabled": true`; `main.ts` `onDragDropEvent` needs it to get
  real file paths. On Windows this swallows HTML5 `dragover`/`drop` inside the webview, so HTML5 DnD
  is not usable without breaking file drop → use pointer events.
- Custom layouts are already blocked on reorder: `layoutIsStale()` compares `titlesKey` (paths in
  order) → `canConvert` false + hint in the custom menu tab. Do NOT clear the layout (would destroy the
  user's editor work; sorting back makes it valid again). `renderAll()` → `refreshLayoutCheck()` covers it.
- `moveSelected(delta)` (up/down buttons) exists and stays as the keyboard path (unchanged behaviour).
- Engine rule to mirror: `dvd::check_group_shapes` – merged members need the same audio and subtitle
  track count as the first member.

## Files

- `frontend/src/order.ts` (new, no imports, erasable TS only – Node runs it directly)
- `frontend/src/main.ts`
- `frontend/src/styles.css`
- `frontend/src/locales/{de,en,ru}.json` (new keys, parity enforced by `check-i18n`)
- `scripts/check-order.mjs` (new)
- `package.json` (`build` script)

## Steps

1. `order.ts`
   - `moveItem<T>(arr: T[], from: number, to: number): void` – `splice` out, `splice` in (mutates).
   - `shapeMismatch(groups: { audio: number; subs: number }[][]): Set<[group, member]>` or similar:
     members whose counts differ from the group's first member. Single-title groups never mismatch.
2. `main.ts` – `moveTitle(from, to)`: `state.splitUndo = null`; `moveItem(state.titles, from, to)`;
   moved title `mergeWithPrevious = false` (no silent merge into its new predecessor);
   `normalizeMerge()`; keep `state.selected`; `renderAll()`; `scheduleCalc()`. No-op if from === to.
3. `main.ts` – `renderTitleList`: add `data-id` to each `li.title-item`. Pointer listeners are attached
   ONCE in the init block on `#title-list` (delegation; `runCalc` re-renders the list mid-drag):
   - `pointerdown` on a card (not on `.title-merge`/buttons, not while `state.converting`, primary
     button only): remember id + start point.
   - `pointermove`: start dragging after 5 px (`setPointerCapture` on the list, `.dragging` on the
     card, `body.reordering`); target = `document.elementFromPoint(...)?.closest(".title-item")`,
     upper/lower half → `.drag-over-top` / `.drag-over-bottom` (only one card highlighted). Re-query
     cards by `data-id` each move (DOM may have been replaced).
   - `pointerup`: if dragging and a target exists → compute insert index (after removal adjustment)
     → `moveTitle`; clean up classes. Suppress the click that follows a real drag.
   - `pointercancel` / `Escape`: abort, clean up.
4. Merge pre-check: per title compute audio = selected audio tracks, subs = selected AND supported
   subtitles (same counting as `editorTitles()`), groups from `titleGroups()`. Mismatched members get
   class `.merge-bad` and a warning line (new keys, e.g. `main.mergeTrackMismatch` with
   `{audio}`/`{subs}`/`{first}` placeholders) on the card; `canConvert` in `renderFooter` also requires
   no mismatch. No umlauts in `.ts`.
5. `styles.css` (append near `.title-item.merged`):
   - `.title-item.dragging { opacity: 0.4; }`
   - `.title-item.drag-over-top { box-shadow: 0 -3px 0 0 var(--accent); }` and
     `.drag-over-bottom { box-shadow: 0 3px 0 0 var(--accent); }` (box-shadow, not border: no layout
     shift, no clash with `.merged`'s left border)
   - `body.reordering { user-select: none; cursor: grabbing; }`
   - `.title-item.merge-bad` / warning line in the existing error/warning colour.

## Verification

Automated:
- `scripts/check-order.mjs` imports `../frontend/src/order.ts` and asserts with `node:assert`:
  `moveItem` up, down, same index, to first, to last; `shapeMismatch` equal group ok, differing audio
  flagged, differing subs flagged, single titles never flagged. Add to `npm run build` after
  `check-i18n`.
- `npm run build` (check-i18n, check-order, tsc, vite) green.
- `cargo check -p convertx-gui` green.

Manual in `npm run tauri dev` (native drop cannot be automated):
- drag a card up/down → highlight shows the drop side, order and numbers update;
- plain click still selects; merge button still works; up/down buttons unchanged;
- dropping files onto the window still adds videos;
- merged group with different track counts → warning on the card, convert disabled;
- custom layout + reorder → convert disabled with stale hint; sort back → enabled again.

## Done

STATUS.md: remove "Merge: no GUI pre-check of track counts" from known issues, add changelog entry.
