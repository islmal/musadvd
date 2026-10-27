# STATUS — convertx-rust

> State as of **2026-10-27**. Architecture/context only; facts: `SYSTEM_STATE.txt`; build rules: `CLAUDE.md`.

## 1. Mission & Out-of-Scope

Clean-room Rust reimplementation of *VSO ConvertXToDVD 7*: arbitrary videos in → standards-compliant
**DVD-Video** (`VIDEO_TS`, ConvertX-style menus) or **Blu-ray / AVCHD** (`BDMV` + `CERTIFICATE`) out,
optionally burnt. Three deliverables: engine library (`src/`), CLI (`src/main.rs`), German/English desktop GUI
(Tauri 2 + vanilla TS, `src-tauri/` + `frontend/`), plus **Burn Studio** (ImgBurn replacement window).

Principles: **clean room w.r.t. ConvertX** (only its *template files* are read, never its code) – this does
NOT apply to `src/dvdvm.rs`, `src/dvd_mux.rs`, `src/ifo.rs` (**ports of dvdauthor's GPL-2.0 `dvdcompile.c`/`dvdvob.c`/`dvdifo.c`/`dvdpgc.c`**, source-guided, byte-identical to dvdauthor 0.7.2+) `src/bd_mux.rs`/`src/bd_clip.rs` (**Apache-2.0 tsMuxer 2.7.0 derived**: packet scheduling, CLPI/MPLS layout, checked field by field against its output) and `src/spumux.rs`: that is a **port of dvdauthor's GPL spumux**, derived from a Ghidra decompile
of `spumux.exe` (via `rea`) and checked byte for byte against its recorded output; treat it as GPL-derived
when deciding the project licence; **few, proven
black boxes** (only ffmpeg/ffprobe remain external – `spumux.exe`, `dvdauthor.exe` and `tsMuxeR.exe` are gone;
everything else is own Rust: template
parser, compositor, subpicture masks, text subtitle renderer, subpicture muxer, navigation, bitrate model, UDF 1.02/ISO 9660 + UDF 2.50 writers,
layer break, IMAPI2 burner; no mkisofs/libcdio); **deterministic authoring** (all disc addresses planned
before writing); **native on Windows** (tools from `bin/` first, `CREATE_NO_WINDOW`); **honest UI**
(unsupported settings flagged, progress + ETA for every long step, cancel kills all children).

**Out of scope / not implemented:** BD-J, Blu-ray pop-up menus, text outlines, template *outro* animation, HEVC/UHD output (HEVC is input only), custom size for BD, multi-session, 8.3 camcorder AVCHD layout
(ours = BDMV on DVD media), MP2/TrueHD/E-AC-3 output, LPCM on DVD-Video, multi-angle, seamless
branching, DVD-Audio, CSS, region coding, DVD-R DL layer break (drive default).

## 2. System Architecture & Data Flow

**DVD job – five phases** (`eta::JobPhase`, "Phase n/5"): probe (ffprobe JSON) → plan titles (one
titleset per menu entry = merge group) → **[1] menus** (ConvertX template via `template.rs`/`menu.rs`, or custom JSON
layout from the WYSIWYG editor via `layout.rs`; masks → native `spumux.rs`) → bitrate from the *real* menu size →
**[2] videos** (parallel, one ffmpeg run writes MPEG-2 + AC-3 straight into a DVD program stream, piped
through one native subtitle stage per stream; text subs are rendered to bitmaps first) → **[3] authoring** (`author::author_dvd` → `dvdvm`/`dvd_mux`/`ifo` → `VIDEO_TS`; `dvdauthor_xml` only feeds the report and `render-menu`) →
**[4] ISO** (`burner::create_disc_image`: own UDF 1.02 + ISO 9660 bridge, layer break) → **[5] burn**
(IMAPI2 / growisofs). Phases 1–3 = `dvd::build_dvd`; 4–5 = burn dialog (`src-tauri/src/burn.rs`), shared
with the CLI through `burner::burn_video_ts`.

**Blu-ray / AVCHD** (`DiscType::Bd25/Bd50/Avchd5/Avchd9`; `build_dvd` hands over to `bd::build_bd`):
per title libx264 or `h264_nvenc` High@4.1 → MKV → one native mux run per title (`bd_mux::mux_title`: ffmpeg `-c copy` → MPEG-TS pipe → re-packetised BDAV m2ts + CLPI/MPLS by `bd_clip`; own playlist + clip, own
frame rate) → our own `index.bdmv`/`MovieObject.bdmv` (`hdmv.rs`, `bd_menu.rs`) → own UDF 2.50 writer
(`udf250.rs`, metadata partition, no ISO bridge). Menus: same `CustomLayout` on a 1920×1080 canvas;
buttons become an **IGS** stream (`igs.rs`, real ICS segments, muxed on PID 0x1400 with a still-mode playlist). Text subtitles are rendered natively to PGS (`pgs.rs`, `textsub::Canvas::hd`). ConvertX templates reach it through `bd_template.rs`: page
coordinates are scaled to 1920×1080 (4:3 pillarboxed), `ML_*` buttons are translated to `layout::Action`s
(not DVD registers) and `bd_menu::plan` turns those into HDMV commands; a template without pages = no menu. AVCHD = same pipeline on DVD-5/9 media with AVCHD limits.

**Burn Studio**: `burn-studio` window; GUI and CLI share `burner/studio.rs`; sources (file, BIN, read,
on-the-fly image through `ringbuf.rs`) → backend `imapi` (default) or `direct` (`mmcwrite.rs`, own MMC
commands over SPTI) → graph telemetry (`.ibg`). Tauri side (`burn_studio.rs`) only wraps commands/events.

**Live player** ("Schnitt & Bild"): `src-tauri/src/preview.rs` = loopback HTTP server streaming a 480p
8-bit High H.264 fMP4 proxy from ffmpeg (one child, token per file; the file is probed once per token
with the crop applied, a crop change = new token); filters come from `encoder::preview_filter`
(deinterlace, crop, scale, then tonemap for HDR); seek = new stream. Split-by-chapters is GUI state
only (same source, different trim).

**Key design decisions**
- **Template menu intro (BD/AVCHD)**: `[MNUP_INTRO]` keyframes → `Element.intro`; a page is ONE clip
  `[intro 0..T | loop T..T+L]`, marks 0 and T, loop restarted by `PlayPLatMark`; IGS display set at
  PTS = T (≤ `IG_BUDGET`, `fit_budget` degrades). Frames never touch disk: helper ffmpeg → page
  background RGBA on stdout, slot videos likewise, Rust composites (`menu_anim`, rayon) onto it and pipes
  rawvideo to the main encode (`Toolchain::ffmpeg_piped`); loop audio crossfaded. Entry through a
  dispatcher object (GPR3–5,7). Off via `animations`/`menu_animations`/`--no-menu-animations`.
- **Template menu intro (DVD)**: same sprites/compositor as BD, but template pages are not `CustomLayout`s:
  `menu::render_page` redraws each graphic on a scratch layer → `menu_anim::Sprite` (`RenderOptions.intro`). One VOB, two cells (intro, page; `jump cell 2` or `pause=inf`), SPU at T, T = whole GOPs, closed GOP (`-sc_threshold` required).
  **dvdauthor wrote the HLI into every NAV pack, ignoring the SPU start** → the native `fix_vobus` leaves it empty in cell 1 of menus with several cells. Intro replays on paging.
- **Subpicture muxing is fully native** (`spumux.rs`, wired in `Toolchain::spumux` for menus and
  `ffmpeg_spumux_pipeline` for titles; no `spumux.exe`, no fallback – `Unsupported` (odd/oversized masks,
  >16 colours) fails the build, `tests/native_menu_templates.rs` proves all 20 installed templates avoid it).
  Per SPU it inserts a `dvdauthor-data` 0xBE sector (PTS, CLUT as YCrCb, tag 2 group lanes, tag 3
  buttons + nav; dvdauthor reads it instead of an SPU palette) plus the 0xBD sectors at SCR ≥ PTS−150 ms and
  rewrites SCRs (`max(in, prev+43886)`). Copied quirks: pixel values by first raster appearance, even crop,
  transparent slot forced to 0, default nav = nearest button within 90°. Button grouping is a 3^N search
  (`pick_groups`) capped at `MAX_GROUP_TRIALS`; beyond it `TooComplex` aborts the build. Byte-identity
  tests: `tests/spumux_native.rs` against **recorded** spumux output (MD5 + small input streams in
  `tests/fixtures/spumux/`; re-record with `SPUMUX_RECORD=<spumux.exe>`, see `tools/oracle/README.md`; dvdauthor fixtures: `DVDAUTHOR_RECORD=<dvdauthor.exe>`, `tests/dvdauthor_native.rs`).
- **Text subtitles** (`textsub.rs`): ffmpeg extracts SRT, `textsub` parses and renders each cue (bundled DejaVu
  Sans in `assets/fonts/`, 31 px, 2 px outline, white/black, hard edges, bottom centre, wraps at 600 px) into
  RGBA pictures that take the bitmap path (`subpic::render` → `<spu>` XML → native mux). Geometry measured
  on spumux/Arial output; `tests/textsub_reference.rs` keeps those numbers with tolerances (glyphs differ),
  `tests/textsub_golden.rs` pixel goldens (`UPDATE_GOLDEN=1`). No bidi/shaping: Arabic/Hebrew/Persian/Urdu/
  Yiddish text tracks on DVD get a build note (`note_rtl_subtitle`); markup (`<i>`, `{n8}`) is stripped.
- Audio (`encoder::plan_audio`, `audio_mode` auto/ac3/lpcm): compliant AC-3/DTS are stream-copied, the
  rest → AC-3; real per-track kbps feed the bitrate plan. LPCM only on BD (`pcm_s24le` → `pcm_bluray`)
  and AVCHD (stereo 16 bit).
- **i18n**: ONE set of locale files `frontend/src/locales/<code>.json` (dotted keys, `{name}` placeholders, `_meta.name`) serves GUI (`i18n.ts`: `t`, `applyStatic` for `data-i18n*`, `fmtNumber`) and engine (keys `engine.*`, read at runtime from `<exe-dir>/locales`, `<cwd>/locales` or the repo path; `i18n::{tr, tf!}`, `UiLang` from setting `ui_language`). **English is the master** (fallback chain: language -> en -> key; `check-i18n` compares against en.json); default UI language stays `de`. `boot.ts` is the entry of all three windows and awaits `initI18n` first (top-level DOM code is translated; dialogs reopened in the main window must build text lazily). Engine log/phase/stream-note texts of `dvd.rs`/`bd.rs` are localized. Template fixed texts (`engine.tpl.<English text>`) and language names (`lang.<iso639-1>`) also come from the locale files. New language = drop a `<code>.json` (with `_meta.name`) into `locales/`, no recompile (de/en/ru exist); GUI loads via Tauri `get_locale_data`, `save_settings` calls `i18n::reload()`; `UiLang` is a `&'static str` code, `sanitized()` accepts only discovered codes. Quirk: a release exe without the bundled `locales/` next to it shows only keys; each reload leaks the old catalogs (shortcut). `scripts/check-i18n.mjs` (in `npm run build`) enforces key/placeholder parity and no umlauts in `.ts`; a Rust test checks every `tr`/`tf!` key exists. Main window switches live; open editor/Burn Studio windows pick it up on reopen.
- **Track labels** (`probe::{audio,subtitle}_labels`, setting `track_label_mode` smart/language/generic): the engine alone names tracks (tag cleaning `clean_tag`, numbered over ALL streams of the file); GUI shows `displayLabel` and `AudioChoice.label` (+ channel layout of the plan), DVD/BD menus use `LayoutTitle.audio_labels`/`subtitle_labels`. A mode change re-probes loaded titles.
- Trim = ffmpeg *input* options via `MediaInfo::input_args()` in every run; crop right after the
  deinterlacer; EBU R128 loudnorm in two runs (`Toolchain::ffmpeg_report`).
- **Merge groups** (`StreamSelection::merge_with_previous`, `dvd::groups`): encoding stays per file; a
  group is ONE DVD title (one PGC, several `<vob>`, chapters concatenated, every file starts a chapter)
  and, on BD, one playlist/HDMV title per file chained by `JumpTitle` (`bd_menu::plan(…, groups)`; short
  pause). Members need equal audio/subtitle track counts (German `BuildError`); DVD members take the
  first member's aspect, differing audio formats force AC-3 for the whole group. Motion chapter pages take
  their menu audio from the first chapter's own clip (file + start).
- **Video processing** (`encoder::VideoProcessing`, settings `scale_mode/scale_filter/deinterlace`):
  one shared `fit_chain` builds DVD and BD filters; `flags=` per `scale` (default bicubic, was lanczos).
  `source_chain` (deinterlace → HDR tonemap → crop) is the head shared with the live player; HDR (PQ/HLG,
  `VideoStream.hdr`) is tonemapped to SDR before `fps`/scale in every encode.
- Layer break: smallest title-cell start with L0 % 16 == 0 that fits, else zero padding before a titleset IFO + `TT_SRPT` patch.
- Custom-layout navigation registers: DVD `g1` play-all, `g2` titleset dispatch, `g3` VMGM router; BD
  `GPR1` chapter mark, `GPR2` play-all. BD titles are entered with `JumpTitle` (VLC timebar), menus with
  `JumpObject`.
- **BDAV muxer** (`bd_mux.rs`): AUs are written in DTS order; PCR on PID 0x1001 starts 500 ms before the first PTS (first PTS = 600 s), ATS interpolated between PCR packets, padded to 6144 bytes. Video PTS/DTS are regenerated on the exact frame grid (ffmpeg only has ms stamps), audio PTS from sample counts, LPCM re-cut into 5 ms frames. Deviations from tsMuxeR: PCR every ≤3000 ticks (its 7000 gave 105 ms at 23.976), SPS/HRD SEI not rewritten (x264/NVENC already emit them), x264 banner SEI dropped, no T-STD simulation. Oracle: `tests/tsmuxer_native.rs` + `tests/fixtures/tsmuxer/` (playlist byte-identical, CLPI identical except SPN/packet count, PES stamps/PSI identical; re-record with `TSMUXER_RECORD`).
- Encoder choice per BD title: NVENC unless the rate needs `fake-interlaced` (1080i → x264); max 4
  parallel NVENC jobs. DVD MPEG-2 stays software (VBV compliance).

**Quirks (easy to get wrong)**
- ffmpeg input `-ss` + stream-copied audio is *not* cut at the start in this build (N-125875) → trimmed
  titles always re-encode audio (else dvdauthor "Cannot infer pts for VOBU").
- ffmpeg's `dvd` muxer LPCM makes dvdauthor report PTS discontinuities → `lpcm` mode writes AC-3 on DVD.
  dvdauthor audio formats are `ac3`, `mp2`, `pcm`, `dts` (not `lpcm`).
- `-loglevel error` (normal runs) hides loudnorm/cropdetect output; `ffprobe -show_entries` prints
  columns in struct order, not requested order.
- BD has no 1080p25/29.97 → x264 `fake-interlaced` (flagged 1080i); NVENC cannot do that.
- `TT_SRPT` VTS start sector is relative to `VIDEO_TS.IFO` in contiguous DVD order; VTS-internal
  addresses are relative to the VTS IFO. Work-dir marker is `.convertx-work/.convertx-work`.
- Custom DVD layouts: coordinates are TV storage pixels (720 wide, non-square); VTSM menu numbers
  shift by one when a titleset has a dispatcher PGC; buttons must be axis-aligned, ≤ 36 per menu,
  ≤ 128 VM commands per PGC. Image paths in layouts are absolute (not portable).

## 3. Roadmap & Known Issues

**Last recorded test run (2026-10-27):** `cargo test --workspace`: all green (lib 363), `cargo check -p convertx-gui`, Unix burner check and `npm run build` (incl. `check-i18n`) ok. i18n language switch and runtime locales (copy `locales/` next to the release exe, add `xx.json`) not clicked through in the real GUI. Release workflow never run (MSVC vs. local gnu toolchain, ffmpeg URL unverified). Merge UI, merged motion chapter menus and track labels (dual-Spanish MKV, BD settings menu in VLC) not checked by hand.
**Next (prioritised)**
- Plan docs/plans/2026-10-27-native-dvdauthor-tsmuxer.md is done (phases 1–3). Still manual (user): VLC on one BD + one AVCHD folder (seek, chapters, menu loop/intro, text + PGS subtitles, LPCM/DTS); standalone player.
- Known quirk: dvdauthor reads one `audpts` entry past the array for a subpicture that starts after the VOBU (uninitialised there); native assumes 0 (matches the oracle). VOB split at 524,272 sectors only unit-tested, no ≥1 GiB run.
1. Live player hardening **acceptance** in the release window (4K HEVC HDR smooth + colours, crop, DVD source without combing, CSP `media-src` under `http://tauri.localhost`); only then consider `-hwaccel` → docs/plans/2026-10-26-live-player-hardening.md
2. Player checks of the 2026-10-25 work: mixed-rate Blu-ray title chain, AVCHD folder in VLC and burnt
   on DVD+R, DTS/LPCM BD audio; JumpTitle behaviour in VLC 3.0.23 (timebar, Menu key, title end →
   menu/loop/play-all).
3. Burn Studio **hardware session** (needs the user's go + blank media): `drive-info`, read/verify a
   pressed DVD, one small IMAPI write, then `io.backend = direct` on a rewritable disc, and the window in
   real Tauri (capabilities, `--burn-studio`). Gaps: no drag & drop, no per-file destination editing,
   no batch mode.
4. Remaining burn tests: DVD-RW/DVD+RW with old content (erase/overwrite), DVD+R DL with a DVD-9 layer
   break; one real CLI burn; a real BD burn + standalone-player test (incl. NVENC discs, IGS menus).
5. Check native menus and natively rendered text subtitles on a standalone player/VLC (look differs
   from the old Arial/FreeType output); then optional: RTL via `unicode-bidi` + shaping, italic/bold,
   `{n8}` top placement.
6. Polish: show harmonised BD tracks; E-AC-3/TrueHD pass-through; seamless BD join; stream-copy trim;
   DVD LPCM muxer; per-title audio mode; Ctrl+C for `build-dvd`.

**Known issues / limitations**
- Still German in every language: `BuildError`/probe errors, warnings from menu/template/encoder modules, dvdauthor output, Burn dialog texts (`burn.rs`).
- Never verified on any standalone player or real disc: Blu-ray menus, AVCHD, mixed-rate BD, any Burn
  Studio operation, the direct write engine. Single-layer DVD burning and one DVD+R DL write work
  (user reports).
- DVD+R DL via IMAPI failed once with "Featureseite … nicht als aktuell gekennzeichnet" (cause open; the
  layer-break exclusive-access change made the next write succeed).
- Live player: tonemapping on 4K real-time and loopback stream under the release origin unverified; 480p ultrafast proxy, ~0.5–1 s delay per seek, no frame stepping, audio = first selected
  track; split on a Blu-ray violates `bd_single_video` (conversion blocked).
- BD intros: seen in VLC only (not on a standalone player: mark-restart, IG timing); `BuildOptions::default` has
  `menu_animations` off; outro keys ignored.
- BD templates: video slots always move (no stills); captions generic ("Kapitel n"); highlights = caption
  glyphs or overlay mask (invisible image button, `bd_menu::mask_coverage`); 16:9 unless aspect policy is
  4:3; GUI size estimate ignores template menus; explicit video starts are not offset by a trim.
- Blu-ray: short pause between titles (one playlist each); menu clips use title 1's frame rate; PGS
  subtitles on a cropped title keep the uncropped position; titles with fewer tracks get silent/empty
  filler tracks; text subtitles rendered natively (DejaVu, no longer Arial); Top Menu → first title menu.
- Native muxer gaps: button attribute byte always 0; a row ≥ 1440 bits skips that subtitle (spumux aborted).
- Text subtitles on DVD: DejaVu Sans only (no CJK glyphs, `.notdef` boxes), no RTL/shaping, no markup.
- Bitmap subtitles: 3 opaque colours (k-means) + hard transparency; DVB/XSUB/teletext/VobSub-on-BD
  skipped with a warning.
- Half D1/SIF force 4:3; image writer aborts on non-DVD file names in `VIDEO_TS`; the subtitle font path
  is Windows-only; settings files from before 2026-10-15 keep their stored job count.
- Removing/reordering/merging videos breaks a custom layout's title references (`titlesKey` changes,
  conversion blocked until fixed). Merge: no GUI pre-check of track counts, mixed audio formats in a group re-encode all tracks (not only the differing one).

## 4. Milestone Changelog (last 10)
- **2026-10-27 – Native authoring cleanup (plan phase 3):** `.ref-src/` removed; locale keys, `CLAUDE.md`, `tauri.conf.json`, `release.yml` (ffmpeg only) already free of dvdauthor/tsMuxeR. Verified: full `cargo test --workspace`, gui + Unix check, `npm run build`.
- **2026-10-27 – Native BDAV muxer (plan phase 2):** `bd_mux` (ffmpeg TS pipe → m2ts), `bd_clip` (CLPI/MPLS), `pgs` (text → PGS), `Toolchain::ffmpeg_sink`; `tsMuxeR.exe`, `bd_patch`, `Toolchain.tsmuxer`, tsMuxeR locale texts removed. Verified: `tests/tsmuxer_native.rs` (4 cases vs recorded tsMuxeR 2.7.0 output, packet/ATS/PCR/CC/EP-map validators, ffprobe), IG menu playlist byte-identical to the patched oracle, `bluray`, `menu_intro` (packet-level IG timing), full `cargo test --workspace`, gui + Unix check, `npm run build`. Not checked: VLC/standalone playback, ≥4 GB title, T-STD buffers, golden PNG of a 1080p cue (structural test only).
- **2026-10-27 – Native DVD authoring (plan phase 1):** `author::author_dvd` replaces `dvdauthor.exe` (`dvdvm` VM compiler incl. `vm_optimize`, `dvd_mux` VOB scan/NAV packs, `ifo` IFO/BUP/PGC); `Toolchain.dvdauthor`, `strip_intro_buttons`, `bin/dvdauthor.exe` removed, `tools/dvdauthor-win` → `tools/oracle`. Verified: `tests/dvdauthor_native.rs` 7 cases byte-identical to recorded dvdauthor output (title, merged, NTSC, menus+dispatchers, playback options, motion intro, AC-3+DTS+2 SPU), lib tests (355), DVD integration tests (build_dvd, menu_intro, motion_menu, custom_menu, playback_options, dvd_audio, disc_image, burn_flow, real_templates, native_menu_templates, ntsc, resolution, render_menu, process_hygiene), gui + Unix check, `npm run build`. Not checked: ≥1 GiB VOB split, standalone player.
- **2026-10-27 – spumux.exe removed, native text subtitles:** `src/textsub.rs` (SRT parser + `ab_glyph` renderer, bundled DejaVu Sans, no Windows font dependency) feeds the bitmap path; `Stage::External`, the `Toolchain::spumux` fallback/field and the arial copy are gone, `bin/spumux.exe` deleted (also from the installer bundle); spumux build output now only goes to `target/spumux-oracle/` for re-recording fixtures. Verified: `cargo test --workspace` green incl. byte-identity vs recorded spumux output (9 cases), template audit (20 templates, 333 menus, no `Unsupported`), renderer vs spumux measurements, pixel goldens, `bitmap_subs` SRT→VobSub round trip, gui + Unix check, `npm run build`. Not checked on a standalone player; look differs from Arial/FreeType.
- **2026-10-27 – Typed spumux `MenuTooComplex` error path:** `SpumuxError::TooComplex` -> `ToolError::MenuTooComplex` -> `BuildError::MenuTooComplex` (manual `From<ToolError>`), localised via `BuildError::message(ui_lang)` (GUI + CLI) from `engine.err_menu_too_complex`. Verified: new unit test in `dvd.rs`, `cargo test --workspace`, `check-i18n`, `npm run build`, gui + Unix check; not with a real 12-button menu build.
- **2026-10-27 – Native subpicture muxer (hybrid pipeline):** `src/spumux.rs` ports spumux (Ghidra decompile) for menu masks and bitmap subtitles; `spumux.exe` remains for text subtitles + fallback. Verified: 9 golden tests byte-identical to `bin/spumux.exe` (subtitles: overlap/open end/crop/multi-sector; menus: default+explicit nav, opaque palettes, 2-group case), unit tests incl. trial limit, `custom_menu` (dvdauthor accepts it, disc colours checked), `bitmap_subs`, `real_templates`, `menu_intro`, `motion_menu`, `ntsc`, Unix check. Not checked on a standalone player.
- **2026-10-27 – Release workflow + in-app updater:** `.github/workflows/release.yml` (tag `v*` = `tauri.conf.json` version) builds the signed NSIS installer via tauri-action (latest ffmpeg release build fetched from gyan.dev (9.0.1 URL is gone), MSVC) and uploads `latest.json`; `tauri-plugin-updater` checks it at GUI start (`main.ts checkForUpdate`, prompt, passive install). Needs repo secret `TAURI_SIGNING_PRIVATE_KEY` (key in `~/.tauri/convertx-rust.key`, pubkey in `tauri.conf.json`; losing it breaks updates). Never run in CI; update flow untested.
- **2026-10-27 – Runtime-loaded locale files:** no embedded JSON anymore; `UiLang` code struct, cached leaked catalogs, `get_available_languages`/`get_locale_data`, CLI `--ui-lang` any code. Verified: `cargo test --workspace`, gui + Unix check, `npm run build` (hint text absent from bundle); not clicked through in the real GUI.
- **2026-10-27 – i18n German/English/Russian + audio track rename:** see Key design decisions. Own audio names: `StreamSelection.audio_names` -> `MediaInfo.audio_names` -> `probe::audio_labels` (GUI input in the Audio tab, session only, not persisted). Verified: workspace tests, gui + Unix check, `npm run build`, browser smoke test with mocked backend; not by hand in the Tauri GUI or a real conversion; Cyrillic glyphs in template menu fonts unchecked.
