# STATUS — convertx-rust

> State as of **2026-10-25**. Architecture and context only; file tree, line counts, dependencies
> and git state are in `SYSTEM_STATE.txt` (`~/.claude/scripts/generate_state.sh`). Build commands
> and conventions: `CLAUDE.md`. The pre-2026-10-25 long-form STATUS (module tables, full
> changelog since 2026-10-14) is in git history (`git show 5c1154f:STATUS.md`).

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

**Out of scope / not implemented:** BD-J, Blu-ray pop-up menus, ConvertX templates on Blu-ray,
HEVC/UHD output (HEVC is input only), custom size for BD, multi-session, 8.3 camcorder AVCHD layout
(ours = BDMV on DVD media), MP2/TrueHD/E-AC-3 output, LPCM on DVD-Video, multi-angle, seamless
branching, DVD-Audio, CSS, region coding, DVD-R DL layer break (drive default).

## 2. System Architecture & Data Flow

**DVD job – five phases** (`eta::JobPhase`, "Phase n/5"): probe (ffprobe JSON) → plan titles (one
titleset per input) → **[1] menus** (ConvertX template via `template.rs`/`menu.rs`, or custom JSON
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
buttons become an **IGS** stream (`igs.rs`). AVCHD = same pipeline on DVD-5/9 media with AVCHD limits.

**Burn Studio**: `burn-studio` window; GUI and CLI share `burner/studio.rs`; sources (file, BIN, read,
on-the-fly image through `ringbuf.rs`) → backend `imapi` (default) or `direct` (`mmcwrite.rs`, own MMC
commands over SPTI) → graph telemetry (`.ibg`). Tauri side (`burn_studio.rs`) only wraps commands/events.

**Live player** ("Schnitt & Bild"): `src-tauri/src/preview.rs` = loopback HTTP server streaming a 480p
fMP4 proxy from ffmpeg (one child, token per file); seek = new stream. Split-by-chapters is GUI state
only (same source, different trim).

**Key design decisions**
- Audio (`encoder::plan_audio`, `audio_mode` auto/ac3/lpcm): compliant AC-3/DTS are stream-copied, the
  rest → AC-3; real per-track kbps feed the bitrate plan. LPCM only on BD (`pcm_s24le` → `pcm_bluray`)
  and AVCHD (stereo 16 bit).
- Trim = ffmpeg *input* options via `MediaInfo::input_args()` in every run; crop right after `bwdif`;
  EBU R128 loudnorm in two runs (`Toolchain::ffmpeg_report`).
- Layer break: smallest title-cell start with L0 % 16 == 0 that fits; otherwise zero padding in front of
  a titleset's IFO + `TT_SRPT` patch (the only legal place).
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
- IMAPI write speeds are sectors/s (1x DVD ≈ 676.27, 1x BD ≈ 2195.07); `WriteSpeed::Max` = −1.
- Ring buffer cancel must be `ErrorKind::ConnectionAborted` (`Interrupted` is retried forever by
  `write_all`); a ring abort after a burner failure must not mask the burner's real error.
- A file ffmpeg still holds open shows 0 bytes in Explorer (NTFS updates the entry on close).
- Custom DVD layouts: coordinates are TV storage pixels (720 wide, non-square); VTSM menu numbers
  shift by one when a titleset has a dispatcher PGC; buttons must be axis-aligned, ≤ 36 per menu,
  ≤ 128 VM commands per PGC. Image paths in layouts are absolute (not portable).
- `Event` JSON: `{"type":"phase|step|progress|warning|eta","data":…}` (camelCase).

## 3. Roadmap & Known Issues

**Last recorded test run (2026-10-25):** `cargo test --workspace` 370 passed, 0 failed; `npm run build`
green. Not re-run during the STATUS condensation.

**Next (prioritised)**
1. Check the live player in a real built window (only the dev app via CDP so far): 4K HEVC proxy
   performance, audio track choice, split + undo on real projects.
2. Player checks of the 2026-10-25 work: mixed-rate Blu-ray title chain, AVCHD folder in VLC and burnt
   on DVD+R, DTS/LPCM BD audio; JumpTitle behaviour in VLC 3.0.23 (timebar, Menu key, title end →
   menu/loop/play-all).
3. Burn Studio **hardware session** (needs the user's go + blank media): `drive-info`, read/verify a
   pressed DVD, one small IMAPI write, then `io.backend = direct` on a rewritable disc, and the window in
   real Tauri (capabilities, `--burn-studio`). Gaps: no drag & drop, no per-file destination editing,
   no batch mode.
4. Remaining burn tests: DVD-RW/DVD+RW with old content (erase/overwrite), DVD+R DL with a DVD-9 layer
   break; one real CLI burn; a real BD burn + standalone-player test (incl. NVENC discs, IGS menus).
5. Polish ideas: show the harmonised BD track layout before converting; E-AC-3/TrueHD pass-through;
   seamless BD title join; stream-copy trimming; DVD LPCM via another muxer; per-title audio mode; a
   progress step for the loudnorm measuring run; Ctrl+C for `build-dvd`.

**Known issues / limitations**
- Never verified on any standalone player or real disc: Blu-ray menus, AVCHD, mixed-rate BD, any Burn
  Studio operation, the direct write engine. Single-layer DVD burning and one DVD+R DL write work
  (user reports).
- DVD+R DL via IMAPI failed once with "Featureseite … nicht als aktuell gekennzeichnet" (cause open; the
  layer-break exclusive-access change made the next write succeed).
- Live player: 480p ultrafast proxy, ~0.5–1 s delay per seek, no frame stepping, audio = first selected
  track; split on a Blu-ray violates `bd_single_video` (conversion blocked).
- Blu-ray: short pause between titles (one playlist each); menu clips use title 1's frame rate; PGS
  subtitles on a cropped title keep the uncropped position; titles with fewer tracks get silent/empty
  filler tracks; text subtitles rendered by tsMuxeR (Arial, fixed); Top Menu → first title menu.
- Bitmap subtitles: 3 opaque colours (k-means) + hard transparency; DVB/XSUB/teletext/VobSub-on-BD
  skipped with a warning.
- Half D1/SIF force 4:3; image writer aborts on non-DVD file names in `VIDEO_TS`; the subtitle font path
  is Windows-only; settings files from before 2026-10-15 keep their stored job count.
- Removing/reordering videos breaks a custom layout's title references (conversion blocked until fixed).

## 4. Milestone Changelog (last 10)

- **2026-10-25 – Workflow change:** STATUS condensed to 4 sections; file/dependency facts moved to the
  generated `SYSTEM_STATE.txt` (git-ignored); build/conventions moved to `CLAUDE.md`. Verified: script
  run, STATUS ≤ 150 lines.
- **2026-10-25 – Live player, "Kapitel hier setzen", split into titles:** `preview.rs` loopback fMP4
  stream, CSP `media-src http://127.0.0.1:*`; chapter edit + `splitByChapters` with one-step undo.
  Verified: 370 tests green, dev app driven via CDP (play, seek, chapters, split, undo, ffmpeg count).
- **2026-10-25 – Audio passthrough/LPCM, trim/crop/loudnorm, AVCHD, one BD playlist per title:**
  `plan_audio`, `apply_trim`/`apply_crop`, `detect_crop`, AVCHD disc types, per-title tsMuxeR runs with
  own frame rate/encoder, GUI tab "Schnitt & Bild". Verified: 367 tests incl. real tool builds; no player.
- **2026-10-25 – BD menus enter titles with `JumpTitle`** (VLC shows time only for non-interactive
  titles). Verified: 325 root-package tests; not in VLC.
- **2026-10-24 – Burn Studio hang fix:** ring cancel `ConnectionAborted`, real burner errors no longer
  masked, step context in IMAPI errors, layer break without exclusive access. Verified: burner tests
  130/130; DVD+R DL write then succeeded (user report).
- **2026-10-24 – Burn Studio M5:** plan complete as far as possible without hardware. 338/338 tests.
- **2026-10-24 – Burn Studio M4:** shared operations layer `studio.rs`, CLI subcommands (`read-disc`,
  `verify-disc`, `build-iso`, `iso-label`, `ifo-layerbreak`, `drive-info`), Tauri commands/events,
  `burn-studio` window. Verified: 338 tests, browser run with mocked `invoke`.
- **2026-10-24 – Burn Studio M3b:** direct MMC write engine `mmcwrite.rs` (opt-in). 331/331 tests,
  all with a fake drive; never on real hardware.
- **2026-10-24 – Burn Studio M3a:** MMC command layer (IMAPI-Ex + SPTI transports), drive info,
  standby/tray guards, `burn_source` with ring buffer, graph telemetry. 317/317 tests; no real write.
- **2026-10-24 – Burn Studio M2:** generic UDF/ISO/Joliet tree writer, builder, sources, ring buffer,
  reader, verifier; DVD image byte-identical (golden MD5 test). 288/288 tests.
