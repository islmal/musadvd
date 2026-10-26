# STATUS — convertx-rust

> State as of **2026-10-26**. Architecture/context only; facts: `SYSTEM_STATE.txt`; build rules: `CLAUDE.md`.

## 1. Mission & Out-of-Scope

Clean-room Rust reimplementation of *VSO ConvertXToDVD 7*: arbitrary videos in → standards-compliant
**DVD-Video** (`VIDEO_TS`, ConvertX-style menus) or **Blu-ray / AVCHD** (`BDMV` + `CERTIFICATE`) out,
optionally burnt. Three deliverables: engine library (`src/`), CLI (`src/main.rs`), German desktop GUI
(Tauri 2 + vanilla TS, `src-tauri/` + `frontend/`), plus **Burn Studio** (ImgBurn replacement window).

Principles: **clean room** (only ConvertX *template files* are read, never its code); **few, proven
black boxes** (ffmpeg CLI, self-built dvdauthor/spumux, tsMuxeR – everything else is own Rust: template
parser, compositor, subpicture masks, navigation, bitrate model, UDF 1.02/ISO 9660 + UDF 2.50 writers,
layer break, IMAPI2 burner; no mkisofs/libcdio); **deterministic authoring** (all disc addresses planned
before writing); **native on Windows** (tools from `bin/` first, `CREATE_NO_WINDOW`); **honest UI**
(unsupported settings flagged, progress + ETA for every long step, cancel kills all children).

**Out of scope / not implemented:** BD-J, Blu-ray pop-up menus, text outlines, template *outro* animation, intro on DVD, HEVC/UHD output (HEVC is input only), custom size for BD, multi-session, 8.3 camcorder AVCHD layout
(ours = BDMV on DVD media), MP2/TrueHD/E-AC-3 output, LPCM on DVD-Video, multi-angle, seamless
branching, DVD-Audio, CSS, region coding, DVD-R DL layer break (drive default).

## 2. System Architecture & Data Flow

**DVD job – five phases** (`eta::JobPhase`, "Phase n/5"): probe (ffprobe JSON) → plan titles (one
titleset per menu entry = merge group) → **[1] menus** (ConvertX template via `template.rs`/`menu.rs`, or custom JSON
layout from the WYSIWYG editor via `layout.rs`; masks → spumux) → bitrate from the *real* menu size →
**[2] videos** (parallel, one ffmpeg run writes MPEG-2 + AC-3 straight into a DVD program stream, piped
through spumux per subtitle) → **[3] authoring** (`author.rs` → dvdauthor XML → `VIDEO_TS`) →
**[4] ISO** (`burner::create_disc_image`: own UDF 1.02 + ISO 9660 bridge, layer break) → **[5] burn**
(IMAPI2 / growisofs). Phases 1–3 = `dvd::build_dvd`; 4–5 = burn dialog (`src-tauri/src/burn.rs`), shared
with the CLI through `burner::burn_video_ts`.

**Blu-ray / AVCHD** (`DiscType::Bd25/Bd50/Avchd5/Avchd9`; `build_dvd` hands over to `bd::build_bd`):
per title libx264 or `h264_nvenc` High@4.1 → MKV → one tsMuxeR run per title (own playlist + clip, own
frame rate) → our own `index.bdmv`/`MovieObject.bdmv` (`hdmv.rs`, `bd_menu.rs`) → own UDF 2.50 writer
(`udf250.rs`, metadata partition, no ISO bridge). Menus: same `CustomLayout` on a 1920×1080 canvas;
buttons become an **IGS** stream (`igs.rs`). ConvertX templates reach it through `bd_template.rs`: page
coordinates are scaled to 1920×1080 (4:3 pillarboxed), `ML_*` buttons are translated to `layout::Action`s
(not DVD registers) and `bd_menu::plan` turns those into HDMV commands; a template without pages = no menu. AVCHD = same pipeline on DVD-5/9 media with AVCHD limits.

**Burn Studio**: `burn-studio` window; GUI and CLI share `burner/studio.rs`; sources (file, BIN, read,
on-the-fly image through `ringbuf.rs`) → backend `imapi` (default) or `direct` (`mmcwrite.rs`, own MMC
commands over SPTI) → graph telemetry (`.ibg`). Tauri side (`burn_studio.rs`) only wraps commands/events.

**Live player** ("Schnitt & Bild"): `src-tauri/src/preview.rs` = loopback HTTP server streaming a 480p
fMP4 proxy from ffmpeg (one child, token per file); seek = new stream. Split-by-chapters is GUI state
only (same source, different trim).

**Key design decisions**
- **Template menu intro (BD/AVCHD)**: `[MNUP_INTRO]` keyframes → `Element.intro`; a page is ONE clip
  `[intro 0..T | loop T..T+L]`, marks 0 and T, loop restarted by `PlayPLatMark`; IGS display set at
  PTS = T (≤ `IG_BUDGET`, `fit_budget` degrades). Frames never touch disk: helper ffmpeg → page
  background RGBA on stdout, slot videos likewise, Rust composites (`menu_anim`, rayon) onto it and pipes
  rawvideo to the main encode (`Toolchain::ffmpeg_piped`); loop audio crossfaded. Entry through a
  dispatcher object (GPR3–5,7). Off via `animations`/`menu_animations`/`--no-menu-animations`.
- Audio (`encoder::plan_audio`, `audio_mode` auto/ac3/lpcm): compliant AC-3/DTS are stream-copied, the
  rest → AC-3; real per-track kbps feed the bitrate plan. LPCM only on BD (`pcm_s24le` → `pcm_bluray`)
  and AVCHD (stereo 16 bit).
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
- Layer break: smallest title-cell start with L0 % 16 == 0 that fits, else zero padding before a titleset IFO + `TT_SRPT` patch.
- Custom-layout navigation registers: DVD `g1` play-all, `g2` titleset dispatch, `g3` VMGM router; BD
  `GPR1` chapter mark, `GPR2` play-all. BD titles are entered with `JumpTitle` (VLC timebar), menus with
  `JumpObject`.
- IGS muxing trick (`bd_patch.rs`): tsMuxeR drops unknown segments, so the IGS goes in disguised as PGS
  and is patched afterwards (PID 0x1200→0x1400, segment type, PMT, CLPI, MPLS) – tied to tsMuxeR 2.7.0.
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

**Last recorded test run (2026-10-26):** full workspace `cargo test --workspace --no-fail-fast` green
(lib 316); `npm run build` ok. Merge UI and merged motion chapter menus not checked by hand.
**Next (prioritised)**
1. Live player in a real built window (dev app via CDP only): 4K HEVC proxy, audio track, split + undo.
2. Player checks of the 2026-10-25 work: mixed-rate Blu-ray title chain, AVCHD folder in VLC and burnt
   on DVD+R, DTS/LPCM BD audio; JumpTitle behaviour in VLC 3.0.23 (timebar, Menu key, title end →
   menu/loop/play-all).
3. Burn Studio **hardware session** (needs the user's go + blank media): `drive-info`, read/verify a
   pressed DVD, one small IMAPI write, then `io.backend = direct` on a rewritable disc, and the window in
   real Tauri (capabilities, `--burn-studio`). Gaps: no drag & drop, no per-file destination editing,
   no batch mode.
4. Remaining burn tests: DVD-RW/DVD+RW with old content (erase/overwrite), DVD+R DL with a DVD-9 layer
   break; one real CLI burn; a real BD burn + standalone-player test (incl. NVENC discs, IGS menus).
5. Polish: show harmonised BD tracks; E-AC-3/TrueHD pass-through; seamless BD join; stream-copy trim;
   DVD LPCM muxer; per-title audio mode; Ctrl+C for `build-dvd`.

**Known issues / limitations**
- Never verified on any standalone player or real disc: Blu-ray menus, AVCHD, mixed-rate BD, any Burn
  Studio operation, the direct write engine. Single-layer DVD burning and one DVD+R DL write work
  (user reports).
- DVD+R DL via IMAPI failed once with "Featureseite … nicht als aktuell gekennzeichnet" (cause open; the
  layer-break exclusive-access change made the next write succeed).
- Live player: 480p ultrafast proxy, ~0.5–1 s delay per seek, no frame stepping, audio = first selected
  track; split on a Blu-ray violates `bd_single_video` (conversion blocked).
- BD intros: seen in VLC only (not on a standalone player: mark-restart, IG timing); `BuildOptions::default` has
  `menu_animations` off; outro keys ignored.
- BD templates: video slots always move (no stills); captions generic ("Kapitel n"); highlights = caption
  glyphs or overlay mask (invisible image button, `bd_menu::mask_coverage`); 16:9 unless aspect policy is
  4:3; GUI size estimate ignores template menus; explicit video starts are not offset by a trim.
- Blu-ray: short pause between titles (one playlist each); menu clips use title 1's frame rate; PGS
  subtitles on a cropped title keep the uncropped position; titles with fewer tracks get silent/empty
  filler tracks; text subtitles rendered by tsMuxeR (Arial, fixed); Top Menu → first title menu.
- Bitmap subtitles: 3 opaque colours (k-means) + hard transparency; DVB/XSUB/teletext/VobSub-on-BD
  skipped with a warning.
- Half D1/SIF force 4:3; image writer aborts on non-DVD file names in `VIDEO_TS`; the subtitle font path
  is Windows-only; settings files from before 2026-10-15 keep their stored job count.
- Removing/reordering/merging videos breaks a custom layout's title references (`titlesKey` changes,
  conversion blocked until fixed). Merge: no GUI pre-check of track counts, live player ignores the new
  video filters, mixed audio formats in a group re-encode all tracks (not only the differing one).

## 4. Milestone Changelog (last 10)

- **2026-10-26 – Template menu intros on BD/AVCHD:** see Key design decisions. VLC check found ~70 % of graphics
  without intro keys (float `==` after `clip()` in `bd_template.rs` → `same_box`); fixed; chapter menus also skipped the intro (root→chapters counted as "paging": same role+title; now `page_family` of the id). Re-checked in VLC (AVCHD, all menu types): intros work. Also fixed `TitlePlan::clip_at`
  (hard-coded 25 s clamp → `motion_secs`, broke `ntsc::menu_sound_per_video`). Verified: full workspace
  run green, e2e tests, Open_Style intro viewed.
- **2026-10-26 – Item-less main menus (Cinema, TV Series):** no `ML_PLAY_*` buttons on the title page → one
  main page showing title 1; series: episode menu (`MNUS_SERIES_EPISODES`, all titles); else bonus menu
  (titles 2..); chapter/settings buttons jump into title 1's menus (`entry ptt/audio` on DVD). DVD + BD.
  Verified: 299 lib tests, all 19 templates build as DVD + AVCHD, menus viewed; not in a player.
- **2026-10-25 – Merge groups + video processing:** `groups()` shared by DVD/BD, multi-`<vob>` titles,
  chained BD playlists, settings page "Videobearbeitung". Verified: unit + `build_dvd`/`bluray` tests, Unix check.
- **2026-10-25 – ConvertX templates on Blu-ray:** `bd_template.rs` (scaling, `ML_*` → `Action`, pagination).
  Verified: 288 lib tests, all real templates render; buttons/highlights confirmed in VLC.
- **2026-10-25 – Live player, chapter at playhead, split into titles:** `preview.rs` loopback fMP4 stream.
  Verified: 370 tests, dev app via CDP.
