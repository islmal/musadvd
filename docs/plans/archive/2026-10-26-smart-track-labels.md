# Smart track labels (audio/subtitle metadata in GUI and menus)

## Goal
Distinguish tracks of the same language ("Spanisch" vs. "Spanisch - Latino") in the GUI and on
DVD/BD/AVCHD menu buttons (`ML_SET_AUD` / `ML_SET_STT`). Rust is the single source of truth: the
frontend never decides a label, it only shows strings computed by the engine.

## Label rules (`probe.rs`)
- **Tag cleaning at parse time** (`clean_tag`): candidates `title`, then `name` (`handler_name` is
  ignored, MP4 junk). Trimmed; rejected when empty, > 60 chars, digits only, `*Handler`, or when it
  only repeats the language: ISO code (any of B/T/1), German name or **English name** (new English
  column in `LANGUAGES`), case-insensitive. The surviving tag is stored in `AudioStream.title` /
  `SubtitleStream.title` (so the GUI's title column is cleaned too; no struct changes).
- `TrackLabelMode { Generic, LanguageOnly, SmartMetadata (default) }`, `from_setting(&str)`
  (`generic` / `language` / anything else = smart).
- `audio_labels(&MediaInfo, mode)` / `subtitle_labels(&MediaInfo, mode)` → one label per stream of the
  file, **numbered over all streams of the file** (selection-independent, so GUI and disc always agree):
  - Generic: `Tonspur N` / `Untertitel N` (N = position in the file).
  - LanguageOnly: `language_name`.
  - Smart: P1 tag → `Sprache - Tag`; P2 no tag but several streams with the same language name →
    `Sprache (k)` (k = ordinal among those streams); P3 → `Sprache`.
  - Subtitles: ` (forced)` appended in every mode.
- `with_layout(label, layout, mode)`: audio channel suffix `"{label} ({layout})"` in LanguageOnly and
  Smart (also after a tag: same tag with different channels must stay distinct), none in Generic.
  `layout` = `AudioOut::layout_label()` of the actual disc plan.
- Known consequence: a lone *selected* duplicate keeps its source number ("Spanisch (2)").

## Files
| File | Change |
|---|---|
| `src/probe.rs` | `RawTags.name`, English names, `clean_tag`, `TrackLabelMode`, `audio_labels`, `subtitle_labels`, `with_layout`, unit tests (**step 1**) |
| `src/config.rs` | `AppSettings.track_label_mode: String`, `TRACK_LABEL_MODES = ["smart","language","generic"]`, normalised via `choice()` like `audio_mode` |
| `src/layout.rs` | `LayoutTitle { audio_labels, subtitle_labels }` (final strings, disc order) |
| `src/dvd.rs` | `BuildOptions.track_label_mode`; `audio_labels()`/`subtitle_labels()` index into the probe labels by selected stream + `with_layout`; fill `LayoutTitle` labels |
| `src/bd.rs` | `layout_titles()` fills the labels; title report (lines ~271/277) uses the same labels, placeholders stay |
| `src/bd_template.rs` | `ML_SET_AUD` / `ML_SET_STT` use `lt.audio_labels[i]` / `lt.subtitle_labels[i]`; delete `format!("Tonspur {}")` / `"Untertitel {}"` |
| `src/bd_menu.rs`, `src-tauri/src/menu_editor.rs`, `src/layout.rs` tests, `tests/menu_intro.rs`, `tests/real_templates.rs` | new `LayoutTitle` fields |
| `src/main.rs` (CLI) | `--track-labels smart\|language\|generic` |
| `src-tauri/src/main.rs` | `probe_file` reads the mode from settings; `AudioTrack.display_label` / `SubtitleTrack.display_label` (base label); `AudioChoice.label` = `with_layout(base, out.layout_label())` per disc/mode plan; setting → `BuildOptions` |
| `frontend/src/api.ts` | `displayLabel`, `AudioChoice.label`, `track_label_mode` |
| `frontend/src/main.ts` | track rows show `displayLabel`; title summary (476) and editor labels (884/887) use `audioChoice(t, a).label` / `s.displayLabel`; after saving settings with a changed mode, re-run `probe_file` for loaded titles and replace their track labels |
| `frontend/src/settings.ts` | dropdown "Spurnamen in Menüs" (Intelligent / Nur Sprache / Generisch) in "Audio & Untertitel" |

`template.rs` / `menu.rs` need no change (DVD template buttons get their labels via `author.rs`
`MenuKind::Settings`).

## Order
1. `probe.rs` + unit tests (done in the planning session).
2. `config.rs` setting + normalisation test.
3. `LayoutTitle` fields, fix all constructors (`cargo check --workspace`).
4. DVD/BD labels, `bd_template` switch; CLI flag.
5. Tauri DTOs + settings wiring.
6. Frontend (labels, re-probe on mode change, dropdown).
7. Full verification.

## Verification
- `probe.rs` unit tests: P1 tag, P1 tag == German/English name/ISO code → fallback, P2 numbering
  (incl. one tagged + one untagged), P3, unknown language duplicates, garbage tags, `name` fallback,
  Generic/LanguageOnly, forced suffix, `with_layout` per mode.
- `bd_template.rs` test: `ML_SET_AUD` button label = given label; `rg 'Tonspur \{\}' src` empty.
- `config.rs` test: unknown mode → `smart`; settings JSON without the field loads.
- `src-tauri` test: `AudioChoice.label` contains the plan's layout.
- `cargo test --workspace`, `cargo check --lib --tests --target x86_64-unknown-linux-gnu`,
  `npm run build`.
- Manual (STATUS open item): title list with a dual-Spanish MKV; BD template settings menu in VLC.
