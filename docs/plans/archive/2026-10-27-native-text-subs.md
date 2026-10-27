# Plan: native text subtitles, drop `spumux.exe` (2026-10-27)

Roadmap item 5. Goal: no product code path starts `spumux.exe` any more. Text subtitles (SRT) are
rendered with `ab_glyph` into 4-colour subpicture bitmaps and go through the existing bitmap path
into the native muxer (`src/spumux.rs`).

## Current state (verified in code)
- `encoder::run_encode` writes `spumux_textsub_xml(...)` (`<textsub>`) for `SubtitleKind::Text`;
  `spumux.exe` renders it with FreeType from `$HOME/.spumux/arial.ttf`. `dvd.rs` (~l.1643) copies
  `%WINDIR%\Fonts\arial.ttf` there and **fails the whole build** if the file is missing.
- `Toolchain::ffmpeg_spumux_pipeline` has two stage kinds: `Stage::Native` (XML passes
  `spumux::handles_xml`, i.e. `<spu image=…>` bitmaps) and `Stage::External` (`<textsub>`).
- `Toolchain::spumux` (menus) falls back to `spumux.exe` when the native muxer returns
  `SpumuxError::Unsupported` (not 8-bit PNG, >720x576, >16 colours, odd masks, button outside image).
- `subpic::render(&Decoded, &Target, tv, dir, folder)` already turns timed RGBA pictures into
  palette PNGs + `<spu>` XML (`quantize`: ≤3 opaque colours + transparency, drops overlaps, min length).
- `menu.rs` has the font machinery (`load_font`/`font_chain` with Segoe fallback for Cyrillic,
  `place_chars`, `text_width`, `rasterize_text`), all private.
- No SRT parser exists (ffmpeg converts ASS/WebVTT/mov_text to SRT before; tags like `<i>`, `{\an8}`
  can still appear).

## Design
New module `src/textsub.rs`, no new dependency:
1. `parse_srt(&str) -> Vec<Cue { start, end, lines }>`: strips BOM, CRLF/LF, `,`/`.` milliseconds, drops
   empty cues and unparsable blocks (with a warning count), strips markup (`<…>`, `{\…}`).
2. `render_cues(cues, tv, fonts) -> Decoded`: per cue wrap to the safe width (frame width − 2×60),
   fixed size, bottom-centred, margins from today's XML (left/right 60, bottom 40, top 20, height
   `tv.height()-2`), fill + outline (+ anti-alias level) drawn into an `RgbaImage` with exactly the
   colours the 4-colour SPU can hold, then `Picture { start, end, x, y, image, forced: false }`.
   Cues render independently → `rayon` (already a dependency).
3. `encoder::text_subtitle_xml` = `textsub::render_cues` + `subpic::render` with an identity `Target`
   (cues are drawn at DVD raster size already). Same call shape as `bitmap_subtitle_xml`, so text and
   bitmap streams produce identical `<spu>` XML and the same muxer stage.
4. Font access through `menu.rs`: make `load_font`, `font_chain`-style helpers `pub(crate)` and add a
   small `subtitle_fonts()` (Arial first, then the existing fallbacks). A missing Arial no longer
   aborts the build; the fallback chain is used, and a warning is emitted only if no font loads at all.
5. Exact look (font px, outline width, colours, line spacing, wrapping) is **measured, not guessed**
   (step 0), because the dvdauthor source is not in the repo.

## Pipeline changes (`Toolchain::ffmpeg_spumux_pipeline`)
- Every stage is native: remove `Stage::External`, the spawned child, its stderr log task, the `HOME`
  env and the "failing spumux → broken pipe root cause" special case for external children (native
  stage errors keep that ordering logic). Name stays `ffmpeg_spumux_pipeline` (module is still `spumux`).
- `native` probing (`handles_xml`) becomes unconditional; a non-parsable XML is a `ToolError::Failed`.
- `run_encode`: the `Text` branch calls `text_subtitle_xml(...)` instead of `spumux_textsub_xml`;
  `.srt` extraction by ffmpeg stays.
- Return value `Vec<String>` (logs) stays; native stage log is the existing "N subtitles added" line.

## Removing the last `spumux.exe` calls
| Place | Change |
|---|---|
| `tools.rs` `Toolchain.spumux` field + `locate("spumux")` in `detect` | remove (spumux.exe no longer required at start) |
| `tools.rs` `Toolchain::spumux` external fallback | remove; `SpumuxError::Unsupported` becomes `ToolError::Failed` with the reason |
| `tools.rs` `Stage::External`, `run_capture` use for spumux | remove (keep `run_capture` if other tools use it) |
| `encoder.rs` `spumux_textsub_xml`, its test `textsub_xml`, `SUBTITLE_FONT` | remove |
| `dvd.rs` font copy block (~l.1643–1648), import `SUBTITLE_FONT`, `tools.spumux.display()` (~l.1067) | remove |
| `tests/build_dvd.rs:46` (`tools.spumux == bin/spumux.exe`) | drop the assertion |
| comments/docs mentioning spumux.exe (`probe.rs:124`, `src-tauri/src/main.rs:175`, `tools.rs` header, `author.rs`) | reword |
| `tools/dvdauthor-win/build.sh`, `README.md`, `bin/spumux.exe` | see open decision 1 |
| `STATUS.md` (roadmap 5, architecture "hybrid pipeline", known limits), `CLAUDE.md` build notes | update at milestone end |

Risk from dropping the fallback: any mask the native code answers with `Unsupported` used to be
rescued by `spumux.exe` silently and now fails the build. Mitigation = test D below.

## Golden tests
A. **Unit (`textsub.rs`)**: SRT parser (BOM, CRLF, `.`/`,`, multi-line, tags, empty/bad blocks, overlap,
   cue count), wrapping (no line wider than the safe area), bottom-centre placement inside the raster
   for PAL and NTSC, Cyrillic via fallback font, tiny/huge text edge cases.
B. **Rendering goldens (self, deterministic)**: fixed test font + fixed SRT → committed PNG/SPU
   fixtures under `tests/golden/textsub/`; test re-renders and compares pixel-exact. Regenerate with an
   env flag, never silently.
C. **Oracle comparison vs `bin/spumux.exe`** (`tests/textsub_native.rs`, skips if exe/ffmpeg missing):
   same SRT and clip through the old external path and the new one; both outputs decoded with the
   existing VobSub decoder (`subpic::decode_vobsub` via ffprobe packets). Compare: cue count, start/end
   within ±1 frame, bounding box position ±2 px and size ±10 %, fill/outline colour roles, ink overlap
   (IoU) above a threshold set from the step-0 measurement. Byte identity is **not** a goal for text:
   FreeType-hinted and `ab_glyph` coverage differ. The mux itself stays byte-identical (existing
   `spumux_native.rs`).
D. **No-fallback audit**: run all real ConvertX templates (as in `real_templates`) plus `custom_menu`
   through `Toolchain::spumux` with the fallback removed; none may return `Unsupported`/`TooComplex`.
   Also covers roadmap 5's "golden tests with masks from real templates".
E. **End to end**: extend `bitmap_subs.rs` VobSub round trip (SRT → build → build from the VOB) so it
   runs with a `Toolchain` that has no spumux; assert 3 cues and timing as today; `process_hygiene`,
   `cargo check --lib --tests --target x86_64-unknown-linux-gnu` and `check-i18n` stay green.

## Order
0. **Spike/measure** (no product change): spumux.exe on a fixed SRT → decode → record font pixel size
   for `fontsize=28`, fill/outline/shadow colours, outline width, line gap, wrapping, margins, whether
   `<i>`/`<b>` are honoured, how frame snapping behaves. Results go into the module doc.
1. `menu.rs` font helpers `pub(crate)`; `textsub.rs` parser + renderer + unit tests (A).
2. `encoder::text_subtitle_xml` and `run_encode` switch; rendering goldens (B).
3. Pipeline: all-native stages; remove `Stage::External`.
4. Remove spumux fallback/field/font copy/XML generator (table above); fix compile fallout.
5. Oracle test (C), no-fallback audit (D), e2e (E).
6. `cargo test --workspace`, gui check, Unix check, `npm run build`; then `/status`, archive plan.

## Known limits (accepted unless you object)
- **RTL/complex scripts regress**: spumux links fribidi, `ab_glyph` does no shaping/bidi. Arabic/Hebrew
  cues would render in wrong order/unjoined. v1: warn when a cue contains RTL characters; follow-up
  could add `unicode-bidi`.
- Markup beyond stripping (italic/bold/colour/position `{\an8}`) is ignored in v1, unless step 0 shows
  spumux honours it and it is cheap (italic/bold via `font_candidates`).
- Look will differ slightly from today's output (unhinted glyphs); the DVD result is judged on a
  standalone player/VLC by you, not by tests.
- Blu-ray/AVCHD text subtitles stay with tsMuxeR (separate pipeline, out of scope).
- Menus: not re-verified on a standalone player (existing roadmap item).

## Open decisions for you
1. **`bin/spumux.exe` and `tools/dvdauthor-win/build.sh`**: (a, recommended) keep the tracked binary
   only as a test oracle for C and the existing byte-identity tests, never loaded by product code;
   (b) delete it and freeze oracle output as fixtures (loses the live comparison, drops the GPL binary
   from the repo; `src/spumux.rs` stays GPL-derived either way).
2. **Test font for B**: commit a small freely licensed font (e.g. DejaVu Sans, ~750 KB, licence file
   added) for deterministic goldens, or depend on the machine's Arial and skip if absent (fragile).
3. **RTL**: accept the warning-only limitation for now, or include `unicode-bidi` in this plan.
