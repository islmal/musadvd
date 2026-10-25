# Plan: "Zusammenführen" (merge groups) + video processing settings

Date: 2026-10-25 · Status: approved

## Goal

1. Let the user merge several inputs into one playback sequence behind a single menu entry.
2. New "Ausgabe → Videobearbeitung" settings: scaling method, scaling filter, deinterlacing.

## Approved design decisions

- **DVD: a group is ONE title (one PGC) with several `<vob>` entries**, not several titles in the
  same VTS. dvdauthor plays the VOBs in sequence and writes `TT_SRPT` itself. "One menu entry = one
  title" stays true, so play-all (`g1`), dispatcher (`g2`), chapter menus, `return_to_title_menu`
  and layer-break planning need no change. Timeline/chapters run across the group; every merged
  file starts a new chapter.
- **Blu-ray: one playlist per file, navigation chains them** (no tsMuxeR `+` join: that would force
  one frame rate and is untested for PGS/text subs). Known cost: short pause between members.
- **Default scaling filter becomes bicubic** (today `flags=lanczos` is hard-coded) – existing
  projects change output slightly; test strings are updated.
- Encoding stays **per file** (parallelism, bitrate plan, trim/crop unchanged); groups exist only in
  menus, authoring and navigation.

## Data model

- `StreamSelection.merge_with_previous: bool` (`#[serde(default)]`, camelCase `mergeWithPrevious`).
  A group = one input plus all directly following inputs with the flag set; list order = playback
  order. Flag on the first input is ignored.
- `dvd::groups(&[StreamSelection], n) -> Vec<Range<usize>>`, shared by DVD and BD.
- `BuildOptions.video: VideoProcessing { scale: ScaleMode, filter: ScaleFilter, deinterlace: Deinterlace }`
  (Copy struct + enums in `encoder.rs`, `Default` = Letterbox / Bicubic / Auto).

## Steps (in order)

1. **`src/encoder.rs`**
   - Add `ScaleMode { Letterbox, PanScan, Stretch }`, `ScaleFilter { Bilinear, Bicubic, Lanczos }`
     (`fn flag() -> &str`), `Deinterlace { Auto, Yadif, Off }`, `VideoProcessing`.
   - `video_filter(v, aspect, tv, resolution, vp)` and `bd_video_filter(v, rate, resolution, vp)`:
     chain = deinterlacer → user crop → `fps` → `scale=W:H:flags=<filter>:out_color_matrix=…` →
     Letterbox: `pad` (as today, via `active_size`) / PanScan: scale to `cover_size()` then
     `crop=fw:fh` / Stretch: `scale=fw:fh`, no pad → `setdar`/`setsar` → `format=yuv420p`.
   - Add `cover_size(source_aspect, target, frame)` next to `active_size` (fills the frame, the
     overhang is cropped centred).
   - Deinterlace: Auto = `bwdif=mode=send_frame` only if `v.is_interlaced()` (today's behaviour);
     Yadif = `yadif=mode=send_frame` always; Off = no filter. User crop stays directly after the
     deinterlacer (or first) – deinterlacers keep the frame size, crop coordinates stay valid;
     trim stays in `MediaInfo::input_args()`.
   - Per-filter `flags=` is used instead of global `-sws_flags` (explicit `flags=` wins over it).
   - DVD and BD encode job structs get a `video: VideoProcessing` field passed into the filters.
   - Menus/thumbnails/fit filters are not touched.
2. **`src/config.rs`**: `AppSettings` fields `scale_mode` (`letterbox|panscan|stretch`),
   `scale_filter` (`bilinear|bicubic|lanczos`), `deinterlace` (`auto|yadif|off`); const lists,
   defaults, `sanitized()` via `choice()`, mapping onto `opts.video` next to `audio_mode`.
3. **`src/author.rs`**: `TitleSpec.vob: String` → `vobs: Vec<VobSpec { file: String, chapters: Vec<String> }>`;
   XML writes several `<vob file=… chapters=…>` inside one title `<pgc>`. Adapt `parse_cli`,
   `placeholder`, `validate`, existing tests.
4. **`src/dvd.rs`**
   - `groups()`; validation (German `BuildError`): equal audio and subtitle track counts per group,
     ≤ 99 chapters per group.
   - Harmonise per group: all members use the first member's `Aspect`; if audio kinds differ at a
     track position, force AC-3 for that position in the whole group.
   - Menu stage (`render_titleset`, `TitleEntry`s, `main_pages`, `LayoutTitle`s at ~line 1069,
     template path) iterates groups: label/thumbnail/clip/track labels from the first member,
     chapters = concatenation of all members' chapters (each `ChapterEntry` uses its member's
     `chapter_thumbs` / `clip_at`), duration = sum.
   - Titlesets: one `TitlesetSpec` per group, `vobs` from all members in order.
   - Pass `opts.video` to the encode jobs.
5. **`src/bd_menu.rs`** – `plan(layout, titles, traits, opts, groups)`:
   - One HDMV title/playlist per file (unchanged numbering); layout title `g` → first member's
     HDMV title.
   - Non-last members end with an unconditional `jump_title(next member)` (mark reset as today);
     the last member keeps today's end logic, "next" = first member of the next group, loop → first
     member of group 1, single loop → first member of its own group.
   - `PlayChapter{g, n}` resolved at plan time to (member, local chapter mark); `PlayAll` → title 1.
   - `SetAudio`/`SetSubtitle` use the first member's sources (track order is equal by validation).
   - `main_menu`/`own_menu` look up by group index. `plan_without_menus` already chains – unchanged.
6. **`src/bd.rs`**: `layout_titles` / `chapter_samples` return one entry per group (for editor
   layouts and `bd_template::layout_from_template`); pass groups to `bd_menu::plan`; same
   track-count validation as DVD (layout is already harmonised disc-wide); pass `opts.video` to the
   BD encode jobs.
7. **`src/main.rs` (CLI)**: `--scale letterbox|panscan|stretch`, `--scale-filter bilinear|bicubic|lanczos`,
   `--deinterlace auto|yadif|off`, `--merge 2,3` (1-based inputs merged with their predecessor).
8. **`src-tauri/src/main.rs`**: if the conversion input struct is not `StreamSelection` itself, add
   `merge_with_previous` and map it.
9. **Frontend**
   - `api.ts`: `scale_mode`, `scale_filter`, `deinterlace` in `AppSettings`; `mergeWithPrevious` in
     the conversion input.
   - `settings.ts` page `video` ("Videobearbeitung"): selects "Video Skaliermethode"
     (Letterbox (Standard) / Pan & Scan (Zuschneiden) / Strecken), "Skalierungsfilter"
     (Bilinear (schnell) / Bikubisch (Standard) / Lanczos (hohe Qualität)), "Video-Deinterlacing"
     (Automatisch (bwdif) / Immer deinterlacen (yadif) / Nie deinterlacen – hint: may cause comb
     artefacts when interlaced material is scaled).
   - `main.ts`: `Title.mergeWithPrevious` (default false, reset to false for the first title after
     remove/reorder); title-list button "Mit vorherigem zusammenführen" / "Trennen"; members shown
     indented with a bracket; `editorTitles()` returns one entry per group (index = group number,
     chapters/durations/chapter starts concatenated with offsets); `startConversion` sends the flag.
   - `styles.css`: group indentation/bracket.

## Automated verification (mandatory)

- `encoder.rs` unit tests: exact filter strings for Letterbox / PanScan / Stretch on DVD PAL+NTSC
  and BD; one per filter flag; deinterlace Auto (interlaced/progressive), Yadif on progressive,
  Off on interlaced; with user crop: `crop=` directly after the deinterlacer / first. Update
  existing `flags=lanczos` assertions to the bicubic default.
- `config.rs` unit tests: unknown values sanitise to defaults; mapping to `VideoProcessing`.
- `dvd.rs` unit test: `groups()` with flags on first / middle / last input.
- `author.rs` unit test: a title with two VOBs → one `<pgc>` with two `<vob>` each with own `chapters=`.
- `bd_menu.rs` unit tests: groups `[1,2]` + `[3]`: `PlayTitle{2}` → `jump_title(3)`; member 1 ends
  with `jump_title(2)`; `PlayChapter` across the member boundary; end behaviour with
  `play_sequentially` and `loop_playback`.
- `tests/build_dvd.rs`: two short lavfi inputs merged → only `VTS_01_*` (no `VTS_02_*`), one
  `<pgc>` with two `<vob>` in `authoring_xml`; skip when tools are missing.
- `tests/bluray.rs`: two merged inputs with a menu layout → navigation chains both playlists;
  skip when tools are missing.
- Full: `cargo test --workspace`, `npm run build`, `cargo check --lib --tests --target x86_64-unknown-linux-gnu`,
  `tests/process_hygiene.rs` green.

## Out of scope / known risks

- No GUI pre-check for track-count mismatch (engine reports it in German).
- Live player proxy ignores the new filters.
- No seamless BD join; short pause between members.
- Grouping changes `titlesKey`, so an existing custom layout is flagged stale.
- Not verified on a standalone player.
