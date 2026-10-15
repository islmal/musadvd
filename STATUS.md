# STATUS — convertx-rust

> Single source of truth for handing this project to another contributor (human or AI).
> State as of **2026-10-15** (incl. menu editor). Everything marked ✅ was built **and** exercised (tests or real
> runs); 🟡 is implemented but not verified end-to-end; ⛔ is not implemented.
>
> The project is a **git repository** since 2026-10-15 (branch `main`; the initial commit is
> the baseline of everything built before). This file plus the code comments and the git
> history are the record.

---

## Table of contents

1. [Executive summary & mission](#1-executive-summary--mission)
2. [System architecture & pipeline flow](#2-system-architecture--pipeline-flow)
3. [Implemented modules & current status](#3-implemented-modules--current-status)
4. [External dependencies & tools](#4-external-dependencies--tools)
5. [Completed milestones & verified tests](#5-completed-milestones--verified-tests)
6. [Roadmap & immediate next steps](#6-roadmap--immediate-next-steps)
7. [Appendix: working on this codebase](#7-appendix-working-on-this-codebase)
8. [Changelog](#8-changelog)

---

## 1. Executive summary & mission

**What it is.** `convertx-rust` is a clean-room Rust reimplementation of the workflow of
*VSO ConvertXToDVD 7*: drop arbitrary videos (MKV, MP4, AVI, MOV, TS …) in, get a standards-
compliant **DVD-Video** (`VIDEO_TS`) with ConvertX-style menus out, and optionally burn it to
disc. It consists of:

- a **Rust engine library** (`convertx_rust`, `src/`) that does probing, bitrate planning,
  menu rendering – from the *original ConvertX template INIs* **or** from a custom JSON layout
  designed in the built-in **WYSIWYG menu editor** – encoding, authoring, disc-image creation
  and burning;
- a **CLI** (`convertx-rust`, `src/main.rs`);
- a **desktop GUI** (`convertx-gui`, Tauri 2 + vanilla TypeScript, `src-tauri/` + `frontend/`),
  German UI.

**Scope reality check (important for the next contributor).** The mission statement this file
was requested with mentions Blu-ray/`BDMV`, H.264 output and LPCM. **None of those exist.**
The output format is **DVD-Video only**: MPEG-2 video + AC-3 audio + DVD subpictures. H.264,
HEVC etc. are supported as *input* only. See [§6](#6-roadmap--immediate-next-steps).

**Core philosophy (as actually practised):**

- **Clean room.** No ConvertX code or binaries are used; only its *template files* (INI + PNG
  assets in `C:\ProgramData\VSO\ConvertXToDVD\7\Templates`) are read, parsed by our own parser.
  The custom layout path (menu editor) needs no ConvertX files at all.
- **Few black boxes, and only proven ones.** Heavy codecs are delegated to the `ffmpeg` CLI;
  DVD IFO/VOB structure to `dvdauthor`, subpicture muxing to `spumux` (both self-built static
  Windows binaries). Everything else is our own Rust: template parser, menu compositor,
  subpicture masks, navigation/command generation, bitrate model, **UDF 1.02 + ISO 9660 image
  writer**, **dual-layer break calculation**, **IMAPI2 burner**. No mkisofs/genisoimage/libcdio.
- **Deterministic authoring.** All addresses of the disc image are planned before writing
  (`plan_image`), the layer break is computed on exact sector addresses, DVD file order is
  fixed, menus are rendered from data, not screenshots.
- **Native and self-contained on Windows.** Tools are found in `bin/` next to the exe first;
  WSL/MSYS are not needed at runtime. Every child process starts with `CREATE_NO_WINDOW`.
- **Honest UI.** Unsupported settings are stored but flagged ("noch nicht unterstützt");
  every long step reports progress and a remaining-time estimate; cancel kills all children
  and removes the work directory.

---

## 2. System architecture & pipeline flow

### 2.1 Workspace layout

```text
C:\convertx-rust
├─ Cargo.toml            workspace root ([".", "src-tauri"], default-members ["."]) + engine/CLI package
├─ src/                  engine library (lib.rs) + CLI (main.rs)
│  └─ burner/            disc image writer, layer break, IMAPI2 / growisofs burners
├─ tests/                integration tests (real ffmpeg/dvdauthor/templates; skip if missing)
├─ src-tauri/            Tauri 2 desktop app (Rust side: commands + events)
├─ frontend/             GUI (index.html, src/*.ts, styles.css) – built by Vite into dist/
├─ bin/                  dvdauthor.exe, spumux.exe (self-built, tracked), ffmpeg.exe, ffprobe.exe
│                        (user-supplied, ~220 MB each, git-ignored: copy them in after cloning)
├─ tools/dvdauthor-win/  build script + shims to rebuild dvdauthor/spumux natively (MSYS2)
├─ package.json          Vite 7 / TypeScript 5.9 / Tauri CLI 2.12
```

### 2.2 End-to-end pipeline (one job)

The user-visible job has **five phases** (`eta::JobPhase`), shown as "Phase n/5" in the GUI:

```text
 inputs (any ffmpeg-readable video)
   │
   ▼  probe.rs ─ ffprobe JSON → MediaInfo (video/audio/subtitle streams, chapters)
   │
   ▼  dvd.rs: plan titles (one titleset per input), stream selection, aspect (4:3/16:9),
   │          chapters (source chapters or automatic every N s), labels
   │
 [1/5] MENUS ─ either (a) ConvertX template: thumbnails (ffmpeg) → menu.rs renders every
   │           template page (background PNG + highlight/select 4-colour masks + motion layers)
   │         or (b) custom layout (layout.rs): validate → plan navigation → render static
   │           layers + masks; videos stay separate layers
   │         → encoder: still/motion menus (ffmpeg overlay graph; custom: dynamic layer graph)
   │         → spumux muxes the masks as subpicture + button geometry/navigation
   │
   ▼  bitrate: calculator.rs with the *real* menu size → video kbps for DVD-5/DVD-9/custom
   │
 [2/5] VIDEOS ─ per title (parallel, bounded): encoder::run_encode
   │           (SRT extraction → optional 2-pass analysis → ONE ffmpeg run writing MPEG-2 + AC-3
   │            straight into a DVD program stream → piped through spumux per text subtitle)
   │
 [3/5] AUTHORING ─ author.rs writes dvdauthor.xml (VMGM, per-title VTSM, PGC commands,
   │               dispatcher PGCs, playback options) → dvdauthor builds VIDEO_TS
   │             → moved to <output>/<project>/VIDEO_TS, work dir removed
   │
 [4/5] ISO ─ burner::create_disc_image: plan (DVD file order, layer break, padding,
   │          TT_SRPT patch) → write UDF 1.02 + ISO 9660 bridge image
   │
 [5/5] BURN ─ IMAPI2 (Windows) / growisofs|xorriso (Unix): erase RW if allowed, set layer
             break (DVD+R DL), write via IStream, finalize, read back & compare, eject
```

Phases 1–3 run in the conversion (`dvd::build_dvd`); phases 4–5 run in the burn dialog
(`src-tauri/src/burn.rs`) after a successful conversion (the dialog opens automatically and can
auto-start after a 20 s countdown when a suitable blank disc is present).

### 2.3 Transcoding & remuxing subsystem

- **Orchestration:** `tools.rs` spawns the CLI tools via `tokio::process` (no FFmpeg library
  bindings). `tool_command()` applies `CREATE_NO_WINDOW` (0x0800_0000) + the configured CPU
  priority class, closes stdin, `kill_on_drop(true)`. stdout/stderr are always drained
  concurrently (no pipe-buffer deadlocks).
- **ffmpeg progress/telemetry:** `-progress pipe:2`; stderr lines are parsed into
  `FfmpegProgress { fraction, frame, fps }`; non-progress lines form the error log.
- **Video:** `mpeg2video`, `-threads 0`, yuv420p, GOP 12 (PAL) / 15 (NTSC), `-bf 2`,
  `-dc 9 -qmin 2 -mbd rd -trellis 1`, VBV 1835 kbit, maxrate ≤ 9800 − audio, BT.601 colour
  tags, `-aspect 4:3|16:9`. Filter chain: `bwdif` (interlaced sources) → `fps` →
  `scale` (lanczos, BT.601, tv range) → `pad` (letter/pillarbox) → `setdar` → `format`.
- **Audio:** AC-3 48 kHz, 192 kbps stereo / 448 kbps 5.1 (`ac3_settings`), up to 8 tracks.
  **LPCM, MP2, DTS are not produced.**
- **Mux:** done by ffmpeg itself in the encode run (`-f dvd -muxrate 10080k -packetsize 2048`).
  There is no separate mux pass and no `.m2v`/`.ac3` on disk any more.
- **Subtitles:** text subtitles (SRT/ASS/…) → SRT → `spumux` text rendering (font `arial.ttf`
  copied to `<work>/.spumux/`), chained via pipes: `ffmpeg | spumux -s0 | spumux -s1 > title.mpg`.
  Bitmap subtitles (PGS/VobSub) are not converted (see §6).
- **2-pass:** analysis run to `NUL` with `-passlogfile`, logs cleaned up before/after.
- **GPU decoding (optional, default off):** `Toolchain::probe_hwaccel` tries `cuda`, `d3d11va`,
  `qsv` (Unix: `cuda`, `vaapi`, `qsv`) by decoding 2 s of the input; on success `-hwaccel X`
  is added. Hardware MPEG-2 *encoding* is deliberately not used (VBV compliance).
- **Parallelism:** titles encode concurrently (`run_limited`, semaphore); thumbnails/menu
  encodes too. Cancel (`CancelToken`) aborts all jobs and kills their processes.

### 2.4 Authoring subsystem

- `author.rs` produces `dvdauthor.xml` and the per-menu `spumux` XML. dvdauthor builds the
  IFO/BUP/VOB files; we never write IFOs ourselves **except** patching `TT_SRPT` start
  sectors when padding is inserted for the layer break (`burner/layerbreak.rs`).
- Disc structure: **one titleset per input video**; VMGM holds the title menu (paginated, up
  to the template's slots per page), each VTSM holds root / chapter (paginated) / settings
  (audio + subtitle selection) pages.
- Navigation: button actions (`PlayTitle`, `PlayAll`, `OpenTitleset`, `Menu`, `MainMenu`,
  `PlayChapter`, `SetAudio`, `SetSubtitle`), per-title VMGM dispatcher PGCs (≤ 3 commands
  each), global register `g1` for "play all", command count checked against
  `MAX_PGC_COMMANDS = 128`, no `jumppad`. Playback options: auto-start, play sequentially,
  loop, return to title menu after a title, skip root menu, no title menu for a single title.
- Limits enforced: 99 titles, 99 chapters, 36 buttons/page, 8 audio, 32 subpicture streams.
- **Blu-ray (`BDMV`) authoring does not exist.**

### 2.5 Menu subsystem

- **Template parser** (`template.rs`): Delphi `TIniFile` semantics (case-insensitive,
  UTF-8 BOM, CRLF, comma decimals), nested collections flattened to section paths
  (`[CXT\pages\Item0\buttons\Item3]`), pages/buttons/backgrounds/colour schemes/texts.
- **Compositor** (`menu.rs`): renders each page to a 720×576 (PAL) / 720×480 (NTSC)
  background, composited back to front in template block order; text via `ab_glyph` with font
  fallback chain (Arial → Segoe UI → Segoe UI Symbol); `TextOverflow` = balanced /
  reduce-font-size / truncate. Pages render in parallel (`rayon`).
- **Subpictures:** two 8-bit palette PNG masks per page (highlight / select) with palette
  indices 0 = transparent, 1 = pattern, 2 = emphasis1, 3 = emphasis2 and the scheme's
  colours/alpha → `spumux` (buttons, `x0 y0 x1 y1`, up/down/left/right navigation graph).
- **Motion menus:** pages with video slots become looping clips: an ffmpeg overlay graph
  composites the static layers with scaled preview clips of each title/chapter
  (`motion_menu_args`), duration from template (`main_duration`, default 25 s) or user;
  audio per page (`MenuAudio`: title audio default, silence, or a file; the VMGM is silent
  unless a file is given; VTSM pages use their own video, chapter pages start at their first
  chapter). Pages without slots become audio-only loops when motion + audio are on.
- Templates used in testing: "Open Style" (`10_TitlesPerPageNoRoot.ini`, `Open_Style.ini`
  "Metallic (open style)").

#### Custom menu layouts (menu editor) – `layout.rs`, `frontend/src/menuEditor.ts`

- **JSON model** (`CustomLayout`, camelCase, version 1): `canvas` (design size, default 720×576),
  `highlight` (`color`, `selectColor`, `opacity` – one subpicture palette for all menus) and
  `menus[]`. A menu has `id`, `name`, `role` (`title` = VMGM, "Title" key, the first one is the
  disc start menu; `root` = VTSM of `title` n, "Menu" key), `background` (`color` | `image` |
  `video` {title, startSecs} + `blur` 0–50), `durationSecs`, `audio` (`none` | `title` | `file`)
  and `elements[]` bottom→top: `text` (font, size, color, align, bold, italic, shadow), `image`
  (src, fit stretch/contain/cover), `video` (title, startSecs, fit), each with x/y/w/h,
  `rotation` (0/90/180/270), `opacity` and optional `action`: `playTitle`, `playChapter`,
  `menu` (by id), `playAll`. An element with an action is a button.
- **Validation** (`layout::validate`, also called by the GUI via `check_layout` and before every
  build): ≥ 1 title menu, unique ids, root menus need a valid title, colours, blur/duration/
  opacity ranges, files exist, elements inside the canvas, rotation in 90° steps, **buttons not
  rotated** (DVD hitboxes are axis-aligned `x0 y0 x1 y1`), **≤ 36 buttons per menu**, **no
  overlapping buttons**, targets (title/chapter/menu) exist, every PGC ≤ **128 VM commands**.
- **Navigation** (`plan_navigation`): title menus = VMGM PGCs 1..k; root menus = VTSM PGCs of
  their title. Registers: `g1` play-all flag (shared with the title-end dispatchers), `g2`
  titleset dispatch code (`1..99` chapter, `100+j` j-th root menu; reset by every title PGC
  and every visible root menu), `g3` VMGM router code (`1000+t` play title t, `2000+t` enter
  titleset t). A titleset gets a command-only **dispatcher PGC** (`entry="root"`, VTSM menu 1)
  only if it is entered from outside for a chapter or a non-first root menu; the VMGM gets a
  command-only **router PGC** (after the visible title menus) only if a root menu jumps to
  another titleset. Title end returns to the first title menu that plays the title
  (existing `dispatcher_commands`, so auto start / sequential / loop / return options apply).
- **Rendering** (`build_custom_menus`): static elements are drawn by the template renderer's
  font chain (`menu::text_coverage`, `blit`, `blend_coverage`) into frame-sized transparent
  PNG layers, split at every video element; rotation via `imageops::rotate*`; highlight/select
  masks: glyph shapes for text buttons (index 1, anti-aliased edges index 2), a frame for
  image/video buttons. The canvas is scaled to the TV frame (PAL 720×576 / NTSC 720×480).
- **Encoding** (`encoder::custom_menu_args`): `-filter_complex` built from the layer list –
  background (lavfi `color`, looped still or clip; cover-fit; `gblur=sigma=…`), then each layer
  overlaid in z-order (PNG at 0:0; clips square-pixel-scaled, fitted, `transpose`/`hflip,vflip`
  for 90° steps, `colorchannelmixer=aa=` for opacity, at x/y), sound (title clip / file /
  `anullsrc`). Pages with videos, a video background or sound are motion loops; others stills.
- **Editor** (`menuEditor.ts`): menu manager (add/rename/duplicate/reorder/delete title & root
  menus, start badge, link counts), stage = TV frame shown at 4:3 (CSS scale, so text/picture
  proportions match the TV), drag/resize with handles (snap 2 px, Shift keeps aspect), arrow
  keys, Del, Ctrl+D, Ctrl+Z (60-step history), safe-area guide, live frames of the titles for
  video placeholders (`title_frame`), image previews (`image_preview`), properties panel (text
  styling, fit, rotation – disabled for buttons, opacity, action with title/chapter/menu
  pickers, background/sound/duration, highlight colours), live validation (`check_layout`),
  "Entwurf aus Titeln" (`draftLayout`: title menu pages with 6 video buttons, chapter menus
  with 8 chapters per page), load/save JSON. The main window switches between "ConvertX-
  Vorlage" and "Eigenes Menü (Editor)"; the layout is kept in localStorage.

### 2.6 Burning / output engine

- **Volume label** (`burner/label.rs` + identical `frontend/src/label.ts`): upper case,
  whitespace/punctuation → `_`, everything outside `[A-Z0-9_]` removed, `__` collapsed,
  max 32, underscore-only/empty → `DVD_VIDEO`. Live-sanitised in the UI.
- **Image writer** (`burner/udf.rs`, own code): sectors 0–15 zero, 16 ISO PVD, 17 terminator,
  18–20 BEA01/NSR02/TEA01, 32 main VDS, 48 reserve VDS, 64 LVID, 66/67 path tables, 68+ ISO
  dirs, 256 + last sector AVDP, partition from 257 (FSD, FEs/FIDs, then file data). File data
  contiguous in DVD order (VIDEO_TS.IFO/VOB/BUP, then per VTS: IFO, menu VOB, title VOBs,
  BUP). ISO 9660 records point at the same extents (bridge). CRC-ITU-T tags, descriptor
  version 2, short_ad extents split below 1 GiB. Written to `<project>.iso.part`, renamed on
  success; cancellable.
- **Layer break** (`burner/layerbreak.rs`): parses `VTS_C_ADT` of every VTS IFO for title cell
  starts; picks the smallest cell start with L0 % 16 == 0, L0 ≥ L1 and both layers within
  capacity (DVD±R SL 2,295,104 sectors; DL layer 2,086,912 sectors). If none fits, inserts the
  smallest zero padding in front of a titleset's `VTS_xx_0.IFO` that aligns a cell and patches
  `TT_SRPT` in `VIDEO_TS.IFO` and `.BUP` (VTS-internal addresses are IFO-relative, so this is
  the only place padding is legal).
- **Windows burner** (`burner/windows.rs`, IMAPI2 via `windows` 0.62): enumeration
  (`IDiscMaster2`, `IDiscRecorder2`: vendor/product/volume path), media probe
  (`IDiscFormat2Data`: physical type, blank/appendable/unsupported state, free/total sectors,
  supported write speeds), quick erase of DVD-RW (`IDiscFormat2Erase`), force-overwrite of
  DVD+RW, `SetForceMediaToBeClosed`, `SetWriteSpeed`, layer break for DVD+R DL via
  `IDiscRecorder2Ex::SendCommandSendDataToDevice` (SEND DVD STRUCTURE 0xBF, format 0x20, L0
  data zone capacity), data written through our own COM `IStream` (`#[implement(IStream)]`)
  that yields progress and aborts on cancel, verification by raw reading `\\.\X:` and
  comparing with the image, eject.
- **Unix burner** (`burner/unix.rs`): drives from `/sys/block/sr*`, media via
  `xorriso -outdev … -list_speeds -toc`, burn with `growisofs -dvd-compat [-speed]
  [-use-the-force-luke=break:L0] -Z dev=iso` or `xorriso -as cdrecord -dao`, RW blanking
  with `xorriso -blank as_needed`, verification via the device node.
- **Speeds:** IMAPI reports sectors/s; 1x DVD = 1,385,000 B/s ≈ 676.27 sectors/s. Default:
  DVD-5 = max, DVD-9 = fastest offered speed in 2.3x–4.1x (else slowest). Setting
  `burn_speed` (`auto|max|16|12|8|6|4|2.4`) maps to the nearest supported speed ≤ wish.
- **Auto-start safety:** the 20 s countdown only runs for a blank disc (or a rewritable one
  when `auto_erase_rewritable` is on) with enough capacity and the right layer count; any
  pointer/key/wheel interaction cancels it for good; the disc is re-probed at 0 s.

### 2.7 Remaining-time model (`eta.rs`, `burner::BurnEta`)

- Encoding: `FrameEta` = all output frames (×2 for 2-pass) vs. the summed frame rate of the
  running encodes; ffmpeg's `fps` (a since-start average) only seeds a job's rate, afterwards
  Δframes/Δt (≥ 0.5 s windows) with exponential smoothing (0.85/0.15).
- Authoring ≈ 2 s + bytes / 200 MB/s; image ≈ 1 s + bytes / 150 MB/s.
- Burn = bytes / (effective x · 1.385 MB/s) + 45 s fixed (+60 s for DL) + verify read at ~8x.
  Effective x = x up to 8x, above `8 + (x−8)·0.4` (CAV ramp; 16x ≈ 11.2x average).
- In a running phase the elapsed time is extrapolated over the fraction done; phases without
  progress (finalizing) count down from their estimate.

---

## 3. Implemented modules & current status

Line counts as of this writing (total ≈ 15,600 lines incl. tests and frontend).

### 3.1 Engine (`src/`)

| File | Lines | Responsibility | Status / completed | Pending |
|---|---:|---|---|---|
| `lib.rs` | 13 | Module list | ✅ | — |
| `template.rs` | 621 | ConvertX template INI parser (pages, buttons, backgrounds, colour schemes, texts) | ✅ all "Open Style" templates parse | Other template styles only smoke-tested via the folder listing |
| `calculator.rs` | 279 | Bitrate model: `video = (target·8192 − audio·t − overhead)/t`, clamp 1000–8500 kbps and to 9800 − audio; `DiscType {Dvd5 4300 MB, Dvd9 7950 MB, Custom}`; `Quality` good > 2500 / medium / poor < 1200 | ✅ | Encoding profiles SP/MP/LP not modelled |
| `probe.rs` | 422 | ffprobe JSON → `MediaInfo` (video, audio, subtitles with kind text/bitmap, chapters), ISO 639 helpers | ✅ | — |
| `tools.rs` | 493 | Tool lookup (`bin/` next to exe → `./bin` → `PATH`, candidates verified with `-version`; tolerant of a stray `"` in PATH), `tool_command` (no window, priority, kill-on-drop), `ffmpeg`/`ffmpeg_with` (telemetry), `ffmpeg_spumux_pipeline`, `spumux`, `dvdauthor`, `probe_hwaccel` | ✅ | — |
| `encoder.rs` | 921 | ffmpeg argument builders: `EncodeJob` (filters, codec, 2-pass, direct DVD PS output, SRT extraction, hwaccel), `run_encode`, thumbnails, `motion_menu_args`, `menu_still_args`, **`custom_menu_args`** (dynamic layer graph: `MenuBackground`, `MenuLayer`, `Fit`, `CustomMenuMedia`), `spumux_textsub_xml`, `MenuAudio` | ✅ | `-trellis 1` costs ~40 % encode time (quality choice, no "fast" switch yet) |
| `menu.rs` | 1369 | Page rendering (backgrounds, texts, masks, motion layers/slots), pagination, parallel rendering, `RenderOptions`, `MenuPageFiles` (+ `custom` media); crate helpers `text_coverage`, `blit`, `blend_coverage` for custom layouts | ✅ PAL + NTSC, still + motion | Half-D1/SIF menus n/a |
| `layout.rs` | 1337 | **Custom menu layouts** (menu editor JSON): model, `validate`, `plan_navigation` (dispatchers, router, register scheme), `build_custom_menus` (layers, masks, spumux XML, `MenuSpec`s) | ✅ unit tests + real builds (PAL/NTSC) accepted by dvdauthor | Navigation not yet tried on a real DVD player/VM; text lines are cut with "…" (no word wrap) |
| `author.rs` | 1312 | `TvStandard`, `PlaybackOptions`, `MenuLinks`, `ButtonAction` (incl. `Commands` for ready-made command strings) → DVD VM commands, dispatcher PGCs, command-only menu PGCs (`MenuSpec::commands`), per-menu `pre`, `DvdLayout::title_pre`, `dvdauthor_xml`, `spumux_xml`, `TitleSpec`, limits | ✅ verified with real dvdauthor | No seamless-branching/angles |
| `dvd.rs` | 1713 | Orchestrator `build_dvd`: planning, menus (template path **or** `BuildOptions::custom_layout` → `MenuStage`), thumbnails, menu encoding, bitrate, title encoding (parallel, telemetry), dvdauthor, project folder, work-dir handling, cancel, `Event` stream (`phase`, `step`, `progress`, `warning`, `eta`), `BuildOptions`, `BuildReport` | ✅ | Pipeline order is Menus → Videos (bitrate needs the real menu size) |
| `eta.rs` | 292 | `JobPhase` (5 phases), `Eta` event payload, `FrameEta`, throughput/burn formulas, `format_remaining` | ✅ unit-tested; plausibility-checked on real builds | Early estimate is pessimistic during ffmpeg start-up |
| `config.rs` | 462 | `AppSettings` JSON (`%APPDATA%\convertx-rust\settings.json`), `sanitized()`, `apply_to(BuildOptions)`; menu, TV, encoding, chapters, languages, **burn** settings | ✅ backward compatible (missing fields → defaults) | `Half D1`, `SIF`, `SP/MP/LP` stored but unsupported (`UNSUPPORTED_CHOICES`) |
| `main.rs` | 506 | CLI: `template`, `bitrate`, `render-menu`, `build-dvd` (`--template` **or** `--layout <JSON>`, `--two-pass --motion-menu --motion-duration --motion-silent --menu-audio --tv pal\|ntsc --project-name --jobs --keep-work --hw-decode` …), `drives`, `make-iso` | ✅ | No CLI `burn` command (burning is GUI-only) |
| `burner/mod.rs` | 446 | Types (`DriveInfo`, `MediaType`, `WriteSpeed`, `MediaInfo`, `BurnPhase`, `BurnProgress`, `BurnOptions`, `BurnError`), trait `OpticalBurner`, `system_burner()`, speed helpers, `BurnEta`, `create_disc_image`, `verify_against_image` | ✅ unit-tested | — |
| `burner/label.rs` | 58 | Volume label sanitiser | ✅ | — |
| `burner/udf.rs` | 904 | UDF 1.02 + ISO 9660 bridge image planner/writer | ✅ mounted by Windows + validated with `isoinfo`/`udfinfo` | No multi-extent ISO 9660 files > 4 GiB (VOBs are ≤ 1 GiB, so irrelevant for DVD-Video) |
| `burner/layerbreak.rs` | 289 | Cell-start parsing, layer-break choice, padding + `TT_SRPT` patch, `plan_disc` | ✅ logic + consistency tests | Seamless flags of cells are not considered; real DL media never burnt |
| `burner/windows.rs` | 496 | IMAPI2 burner (see §2.6) | ✅ enumeration/probing verified on real hardware; ✅ write, finalize, eject, read-back verification and burn ETA **reported working by the user** on the Pioneer BDR-209M (2026-10-15); 🟡 erase of RW media and the DL layer break never run | Event-based IMAPI progress (DDiscFormat2DataEvents) not used |
| `burner/unix.rs` | 257 | growisofs/xorriso burner | 🟡 compiles for `x86_64-unknown-linux-gnu`; never run | Its unit tests only compile-checked |

### 3.2 Desktop app (`src-tauri/`, Tauri 2.12)

| File | Lines | Responsibility | Status |
|---|---:|---|---|
| `src/main.rs` | 702 | Commands `probe_file`, `list_templates`, `template_preview`, `calculate_project`, `start_conversion` (with optional `customLayout`), `cancel_conversion`, `default_output_dir`, `startup_files`, `get_settings`, `save_settings`, `default_settings`, `settings_info`, `title_frame` (video frame as data URL for the editor); events `convert-event` (engine `Event`, incl. `eta`), `convert-done` (with `videoTs`, `projectName`), `convert-cancelled`. Conversion runs on its own thread + Tokio runtime. | ✅ |
| `src/burn.rs` | 474 | Commands `list_drives`, `disc_needs`, `probe_media(driveId, videoTs, speedPref, autoErase)`, `sanitize_label`, `start_burn`, `cancel_burn`, `startup_burn` (GUI started with a DVD folder argument opens the burn dialog), `open_folder`; events `burn-progress` (`BurnProgress` + `eta`), `burn-done`. Media view with problem/warning/needs-erase/auto-start rules and per-speed duration estimates; ISO at `<output>/<project>.iso` (numbered if taken); safe post-burn deletion (only folders containing nothing but `VIDEO_TS`/`AUDIO_TS`). | ✅ logic unit-tested; ✅ real burn reported working by the user |
| `src/menu_editor.rs` | 99 | Commands `check_layout` (validation against the project's titles), `load_layout`, `save_layout`, `image_preview` (images only, ≤ 25 MB) | ✅ unit-tested |
| `tauri.conf.json` | — | Window, CSP, NSIS bundle with `bin/*.exe` as resources | 🟡 NSIS bundle never built; release exe built with `--features tauri/custom-protocol` |

### 3.3 Frontend (`frontend/`, vanilla TS + Vite 7)

| File | Lines | Responsibility | Status |
|---|---:|---|---|
| `index.html` | — | Shell, conversion overlay (5 phase chips, total/step bars, ETA line, log, cancel, "Auf DVD brennen …") | ✅ |
| `src/main.ts` | 1184 | Title list (drag & drop), per-title audio/subtitle/chapter/label editing, menu tab with **"ConvertX-Vorlage" / "Eigenes Menü (Editor)"** switch (template picker with preview, or custom layout summary + live validation), `editorTitles()` (index, label, path, chapter count and source chapter names of the live project), layout↔videos binding (`customLayoutKey` in localStorage), disc fill gauge, convert/cancel (blocked while the layout is invalid or made for other videos), ETA display, opens burn dialog after success | ✅ screenshot-verified |
| `src/menuEditor.ts` | 1095 | WYSIWYG menu editor (see §2.5): menu manager, 4:3 stage with drag/resize, properties, actions, validation, draft, load/save; opens a fresh draft when the stored layout was made for other videos (old one reachable via undo); video badges and chapter pickers show the real title labels / chapter names | ✅ screenshot-verified (open, select, drag, add text, apply; 4-title project) |
| `src/layout.ts` | 301 | Layout types (mirror of `layout.rs`), fonts list, `draftLayout` generator for the project's real titles (`EditorTitle.index`, labels, chapter counts, source chapter names), `titlesKey`, `madeForOtherTitles`, `chapterLabel`, helpers | ✅ 4-title draft passes `check_layout` and a full build with dvdauthor |
| `src/settings.ts` | 610 | "Allgemeine Einstellungen" modal (tree: Allgemein, Ausgabe › TV Format/Enkodierung/Videobearbeitung, Menü Optionen, Kapitel, Untertitel, Audio, **Brennen**) | ✅ |
| `src/burn.ts` | 556 | Burn dialog: drive list, media indicator, label (live sanitised), speed list with "(empfohlen)" + duration estimate, checkboxes (verify, keep ISO, eject, delete after, erase confirmation), 20 s countdown, phases Image/Formatieren/Schreiben/Abschließen/Überprüfen, live ETA "[4/5]/[5/5]", two-click cancel, opens Explorer when closed without burning (unless quiet) | ✅ UI verified without disc; ✅ burning reported working by the user |
| `src/label.ts` | 23 | Volume-label rules (same as Rust; cross-checked with identical cases) | ✅ |
| `src/api.ts` | 341 | Typed `invoke`/`listen` wrappers and DTOs (+ editor commands) | ✅ |
| `src/styles.css` | 1588 | Light/dark theme tokens, layout, menu editor | ✅ |

### 3.4 Native tool build (`tools/dvdauthor-win/`)

`build.sh` builds static `dvdauthor.exe`/`spumux.exe` from **ldo/dvdauthor commit `fe8fe35`**
in MSYS2 (MinGW-w64) with shims `wincompat.h`, `include/langinfo.h`, `include/netinet/in.h`;
`README.md` documents it; GPL licence text is shipped as `bin/dvdauthor-COPYING.txt`.

---

## 4. External dependencies & tools

### 4.1 Runtime tools

| Tool | Version in use | Required | Notes |
|---|---|---|---|
| `ffmpeg` / `ffprobe` | 9.0.1 full build (gyan.dev), in `bin/` | yes | Must be the real binaries (scoop shims without `.shim` are rejected by the `-version` check). Needs `mpeg2video`, `ac3`, `dvd` muxer, `lavfi`; `libx264` only for tests. |
| `dvdauthor` / `spumux` | self-built from ldo/dvdauthor `fe8fe35`, static, in `bin/` | yes | Rebuild with `tools/dvdauthor-win/build.sh`. A WSL dvdauthor exists on the dev machine but is unused. |
| IMAPI2 (Windows) | Windows 11 built-in | for burning | COM, CLSIDs `MsftDiscMaster2`/`MsftDiscRecorder2`/`MsftDiscFormat2Data`/`MsftDiscFormat2Erase`. |
| growisofs / xorriso | — | Unix burning only | Optional fallback. |
| mkisofs / genisoimage / libcdio | — | **no** | Replaced by our own UDF/ISO writer. `genisoimage` + `udftools` were installed in the dev machine's WSL **only to validate** images (`isoinfo`, `udfinfo`). |
| Fonts | `arial.ttf`, `segoeui.ttf`, `seguisym.ttf` from `%WINDIR%\Fonts` | yes | Menu text + subtitle rendering. |
| ConvertXToDVD 7 templates | `C:\ProgramData\VSO\ConvertXToDVD\7\Templates` | yes (for menus) | Read-only input. |

### 4.2 Cargo crates

| Crate | Where | Why |
|---|---|---|
| `serde`, `serde_derive`, `serde_json` | engine, GUI | Settings, ffprobe JSON, IPC DTOs |
| `clap` 4 (derive) | CLI | Command line |
| `tokio` 1 (full) | engine | Async process orchestration, cancellation, bounded parallel jobs |
| `image` 0.25 | engine | Menu compositing, thumbnails |
| `png` 0.18 | engine | 8-bit palette PNGs for spumux masks (same version `image` uses) |
| `ab_glyph` 0.2 | engine | Pure-Rust text rendering |
| `rayon` 1 | engine | Parallel page rendering |
| `regex` 1 | engine | Parsing helpers |
| `windows` 0.62 (+ `windows-core`) | engine, Windows only | IMAPI2 COM: features `Win32_Foundation`, `Win32_Storage_Imapi`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`, `Win32_System_Ole`, `Win32_System_Variant` |
| `tauri` 2, `tauri-build` 2, `tauri-plugin-dialog` 2 | GUI | Desktop shell, file/folder dialogs |
| `base64` 0.22 | GUI | Template preview data URLs |

npm: `@tauri-apps/api` 2.12.3, `@tauri-apps/plugin-dialog` 2.8.2, `@tauri-apps/cli` 2.12.2,
`typescript` 5.9.3, `vite` 7.3.7.

### 4.3 Development environment (the machine this was built on)

- Windows 11 IoT Enterprise LTSC 2024; Rust via rustup, host `x86_64-pc-windows-gnu`
  (cargo 1.99), MinGW from scoop (needed for `dlltool`); target `x86_64-unknown-linux-gnu`
  installed for cross-checking `unix.rs`; Node 24 / npm 11; MSYS2 (scoop) for dvdauthor.
- **PATH caveat:** the user's PATH contains `C:\Program Files\dotnet"` (stray quote) which
  breaks cmd/npm. Shell sessions use this prefix (never change the persistent PATH):
  `$clean = ($env:Path -split ';' | % { $_.Trim().Trim('"') } | ? { $_ }) -join ';'; $env:Path = "$env:USERPROFILE\.cargo\bin;$env:USERPROFILE\scoop\apps\mingw\current\bin;$clean"`
- Hardware: Pioneer BD-RW BDR-209M (USB, drive `F:`), detected by IMAPI2; RTX 4070 SUPER
  (CUDA) + Radeon 780M; 16 logical CPUs.

---

## 5. Completed milestones & verified tests

### 5.1 Test suite (2026-10-15: **135 tests, all green** – engine/integration from the user's full run plus the new settings test, GUI from the last GUI run)

`cargo test --workspace` → engine unit tests 97, GUI unit tests 9, integration tests 29:

| Test file | Tests | What it proves (real tools unless noted) |
|---|---|---|
| unit (`src/**`) | 97 | Custom layouts (settings menus and stream selection, background shading graph, JSON round trip, DVD-limit validation, cross-domain navigation commands, dispatcher command limit, layer/mask/spec rendering, NTSC scaling), custom menu filter graph, parsers, bitrate model, filters/args, XML generation, label rules, CRC/tags/dstrings/extents of the UDF writer, layer-break choice + `TT_SRPT` patch, speed mapping, `BurnEta`, `FrameEta`, config sanitising, ffmpeg progress parsing, verify mismatch detection, IMAPI drive enumeration (real drive, read-only) |
| GUI unit (`src-tauri`) | 9 | Layout save/load round trip and error path, layout check against project titles, media-view rules (auto-start only on suitable media, RW erase rules, DVD-9 needs DL, speed estimates), ISO path numbering, safe post-burn deletion, project calculation |
| `build_dvd.rs` | 1 | Two clips (16:9 5.1 + stereo, German SRT with umlauts, chapters) → full DVD-5 with sub-menus; stream codecs in VOBs |
| `two_pass.rs` | 5 | 1-pass/2-pass args, real encodes produce a DVD program stream (MPEG-2 720×576 + AC-3), no `.m2v`/`.ac3`/pass logs left |
| `motion_menu.rs` | 7 | Motion layers/slots on the real template, overlay pipeline produces DVD MPEG, full build with looping menus, sound start per chapter page |
| `ntsc.rs` | 2 | NTSC 720×480 @ 29.97 for titles, still and motion menus, masks, subtitles; menu sound per video |
| `playback_options.rs` | 3 | Auto start / sequential / loop; single title without title/root menu |
| `custom_menu.rs` | 4 | Settings menu with `setAudio`/`setSubtitle` (2 audio tracks, SRT subtitle), chapter video buttons from each chapter start, vignette/bottomDark/dim shading → real build accepted by dvdauthor; custom layout → full DVD with real ffmpeg/spumux/dvdauthor: blurred image background, video button, rotated image, sound, video background; dispatcher (`if (g2 eq 102) jump menu 3`), chapter dispatcher, VMGM router, `g2 = 0` resets accepted by dvdauthor; menu VOB has MPEG-2 + AC-3 + subpicture, ~3 s loop; mask uses only layout colours; NTSC canvas → 720×480; invalid chapter fails before encoding |
| `cancel.rs` | 1 | Cancel during encoding kills tools and removes the work dir |
| `disc_image.rs` | 2 | ISO of a real DVD **mounted by Windows** (`Mount-DiskImage`): FS = UDF, label correct, every file byte-identical; layer-break plan on injected small capacities lands on a cell start, %16, L0 ≥ L1; forced padding + patched `TT_SRPT` keeps title→VTS addresses consistent and mounts correctly |
| `real_templates.rs` | 2 | All installed "Open Style" templates parse |
| `render_menu.rs` | 2 | Single and paginated menu rendering on the real template |

Integration tests **skip with a message** (not fail) when ffmpeg/dvdauthor/spumux or the
ConvertX templates are missing.

### 5.2 Verified outside the test suite (manual runs, 2026-10-14/15)

- ✅ **Real disc burn (user report, 2026-10-15):** the user burned a disc with the GUI on the
  Pioneer BD-RW BDR-209M (drive F:) and reported that writing via IMAPI2, finalizing, ejecting,
  the read-back verification (raw read of `\\.\F:` compared with the image) and the burn
  remaining-time display all worked. Not recorded: medium type and speed. Not part of the
  report (still 🟡, see §5.3): erasing RW media, the DVD+R DL layer break.
- ✅ **Independent image validation:** `isoinfo -d/-l` (ISO 9660 side, dirs, extents) and
  `udfinfo` (UDF 1.02, integrity closed, VRS/MVDS/RVDS/LVID/anchors at the planned sectors) on a
  CLI-made image; MD5 of extracted files = originals.
- ✅ **No console windows:** release GUI conversion monitored at 100 ms resolution:
  8 tool processes (ffmpeg ×4, spumux ×3, dvdauthor ×1), 0 with a window, 0 new
  `ConsoleWindowClass` windows.
- ✅ **Performance** (2 × 3 min 1080p H.264 → DVD-5, release CLI, 16 threads): 17.2 s before the
  direct-mux change → 15.5 s after → 13.8 s with `--hw-decode` (CUDA picked). Encoding runs
  at ~98 % CPU; the earlier low-CPU stretches were the removed copy passes.
- ✅ **GPU decode benchmark:** 1080p H.264: CPU 7.2 s, d3d11va 6.6 s, cuda 6.5 s; 4K HEVC:
  CPU 2.3 s, d3d11va 3.8 s, cuda 2.5 s → option stays off by default.
- ✅ **ETA plausibility:** 2-pass build: estimate within a few seconds from ~50 % onward;
  pessimistic in the first seconds (ffmpeg start-up).
- ✅ **GUI screenshots:** burn dialog (drive found, "Keine Disc eingelegt", label, speed,
  options), conversion overlay with "Geschätzte Restzeit … (Phase 2/5: Videos encodieren …)".
- ✅ **Menu editor in the release GUI** (screenshots): mode switch, draft from two videos with
  live title frames, button outlines, select/drag (X/Y update, undo enabled), add text,
  backend validation "Layout ist gültig", "Übernehmen" → summary card, "Konvertieren" enabled.
  The GUI's stored state was reset to "ConvertX-Vorlage" afterwards.
- ✅ **WYSIWYG check of the engine:** CLI `build-dvd --layout` with Georgia bold heading + shadow,
  italic caption, Impact text rotated 270° at 80 % opacity, blurred video background, two
  video buttons (cover/contain), Segoe UI text button → frame of the menu MPEG and the
  highlight mask inspected visually: positions, fonts, rotation, blur and masks as designed.
- ✅ `cargo check --workspace` without warnings; `npm run build` (incl. `tsc --noEmit`);
  `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.

### 5.3 Never exercised

- 🟡 Erasing DVD-RW / overwriting DVD+RW.
- 🟡 Setting the layer break on a real DVD+R DL (SEND DVD STRUCTURE 0x20).
- 🟡 The Unix burner at runtime.
- 🟡 Custom-layout navigation on a real DVD player / VM (dvdauthor accepts it and the command
  strings are unit-tested, but no playback emulator was run).
- 🟡 Editor features not clicked through in the GUI: image elements/backgrounds via file
  dialog, audio file, loading JSON, menu reordering/deletion, undo after drag. (Saving JSON,
  root/settings menus and stream actions were clicked through on 2026-10-15.)

---

## 6. Roadmap & immediate next steps

### 6.1 Prioritised next tasks

1. **Remaining burn tests:** single-layer burning works (user report, §5.2). Still open: a
   DVD-RW/DVD+RW with old content (erase/overwrite path) and a DVD+R DL with a DVD-9 project
   (layer break command); record medium type and speed.
2. **IMAPI progress events:** progress is currently derived from `IStream::Read` (data handed
   to IMAPI, which buffers). Consider `DDiscFormat2DataEvents` for true write position and
   a better "finalizing" phase.
3. **DVD-R DL layer break:** currently drive default (warning shown). Would need format 0x21/
   0x22 handling or IMAPI-specific support.
4. **Bitmap subtitles** (PGS/VobSub → DVD subpictures): currently only text subtitles are
   converted.
5. **Unsupported settings:** Half D1 / SIF resolutions, SP/MP/LP encoding profiles (stored but
   flagged "noch nicht unterstützt").
6. **Encoding speed switch:** optional "fast" mode without `-trellis 1` (~40 % faster
   encoding at slightly lower quality).
7. **Parallelism default mismatch:** `AppSettings::max_concurrent_encodes` defaults to *all*
   logical CPUs (16), while the engine's own default (`dvd::default_jobs`) is half. With many
   titles the GUI default oversubscribes (each ffmpeg is multi-threaded). Align to half.
8. **Installer:** build and test the NSIS bundle (`npm run tauri build`), confirm `bin/`
   resources land next to the exe.
9. **CLI `burn` command** (currently GUI-only) for scripted use.
10. ~~Delete the stray `output_menu/` folder; put the project under git~~ – done 2026-10-15.
11. **Custom menus:** test the navigation on a real player (or a DVD VM such as libdvdnav);
    word wrap for texts; per-button highlight colours; "settings" pages (audio/subtitle
    selection) for custom layouts; video elements as square-pixel previews in the editor.

### 6.2 Not implemented at all (out of current scope)

- ⛔ Blu-ray / `BDMV` / AVCHD authoring, H.264 or HEVC output.
- ⛔ LPCM / MP2 / DTS audio output (AC-3 only).
- ⛔ Multi-angle, seamless branching, DVD-Audio, CSS, region coding.

### 6.3 Known limitations, edge cases & caveats

- **Pipeline order:** menus are built before videos because the video bitrate is computed with
  the real encoded menu size. The 5-phase numbering follows the real order
  (Menus 1 → Videos 2 → Authoring 3 → ISO 4 → Burn 5).
- **Layer break:** cell *seamless* flags are not checked (prefers no cell); padding is placed in
  front of a whole titleset (only legal place) — inflates the image by < 32 sectors.
- **Image writer:** files in `VIDEO_TS` are required to follow DVD naming; any other file in
  the folder aborts image creation ("unerwartete Datei"). `AUDIO_TS` is always written empty.
- **Auto-start:** never erases RW discs automatically unless `auto_erase_rewritable` is set.
- **Delete after burn** refuses to delete a project folder containing anything besides
  `VIDEO_TS`/`AUDIO_TS` and only removes `.convertx-work` when its marker file is present.
- **ETA:** the first estimates of the encoding phase are pessimistic; burn ETA uses a CAV
  model and fixed lead-in/closing times; the user reported the burn ETA working on real
  hardware, but its accuracy was not measured.
- **GPU decoding** can be slower than CPU for 4K HEVC with D3D11VA; the probe decodes 2 s of
  every input (adds ~1–2 s per title when enabled).
- **Tool discovery:** a broken `ffmpeg` shim on PATH is skipped with a note; `bin/` wins over
  PATH.
- **Fonts:** subtitle font is `arial.ttf` from `%WINDIR%\Fonts` (Windows-specific path; on
  Unix the build would need a font path).
- **Settings file** compatibility is forward/backward tolerant, but unknown enum values are
  reset to defaults on load.
- **Custom layouts:** coordinates are TV storage pixels (720 wide, non-square); the editor shows
  the stage at 4:3, so pictures look as on the TV, but "contain/cover" fits are computed in
  storage pixels (≈ 6.7 % horizontal stretch on PAL, same as on the TV). Text lines are not
  wrapped; overlong lines end with "…" in the engine and are clipped in the editor. Image paths
  are absolute (a layout file is not portable to another machine). A layout stored in the GUI
  is bound to its video list; with other videos it is treated as foreign (warning, fresh draft
  in the editor). A loaded JSON file has no binding and is checked by references only. A layout refers to titles
  by number: removing/reordering videos makes `check_layout` report broken references, and
  converting is blocked until it is fixed. The Motion-Menu switch has no effect in custom mode.

---

## 7. Appendix: working on this codebase

### Build & test

```powershell
# shell prefix (see §4.3) first
cargo check --workspace
cargo test --workspace                       # ~3 min, needs bin/ tools + ConvertX templates
npm run build                                # frontend → dist/ (tsc + vite)
cargo build --release -p convertx-gui --features tauri/custom-protocol   # GUI exe with embedded frontend
cargo build --release                        # CLI
cargo check --lib --tests --target x86_64-unknown-linux-gnu             # Unix burner compile check
```

A plain `cargo build --release -p convertx-gui` (without `tauri/custom-protocol`) produces an
exe that loads `http://localhost:5173` and shows "page not reachable".

### Useful CLI calls

```powershell
convertx-rust build-dvd --input a.mkv b.mp4 --template "C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini" --output D:\out --motion-menu --hw-decode
convertx-rust drives
convertx-rust make-iso D:\out\Film\VIDEO_TS D:\out\Film.iso --label "Mein Film"
convertx-gui.exe D:\out\Film\VIDEO_TS        # opens the burn dialog for an existing DVD folder
```

### Conventions

- UI text and user-facing errors are **German**; code, comments and identifiers English.
- Comments explain *why*; match the surrounding density.
- Every external process goes through `tools::tool_command` (or `Toolchain::command`).
- Integration tests must skip gracefully when tools/templates are missing.
- Patch scripts run through Bash heredocs can lose backslashes (regexes, `\n`, Windows
  paths) — write such scripts to a file first.

### Key facts that are easy to get wrong

- dvdauthor `TT_SRPT` VTS start sector = offset relative to `VIDEO_TS.IFO` in contiguous DVD
  order; VTS-internal addresses (`VTSM_VOBS` @0xC0, `VTSTT_VOBS` @0xC4, `C_ADT` @0xE0) are
  relative to the VTS IFO. `TT_SRPT` sector pointer at VMG IFO 0xC4; entries of 12 bytes from
  +8, VTS number at byte 6, start sector u32 BE at bytes 8–11.
- The work directory's marker file has the same name as the directory: `.convertx-work/.convertx-work`.
- IMAPI write speeds are sectors per second; `WriteSpeed::Max` is sent as −1.
- `Event` JSON shape: `{"type":"phase|step|progress|warning|eta","data":…}` (camelCase).
- Custom layouts: VTSM menu numbers shift by one when a titleset has a dispatcher PGC
  (`plan_navigation` returns the real PGC numbers in `Placement`); the VMGM title-end
  dispatchers come after the router (`dispatcher_menu(pages, title)` counts all VMGM PGCs).

---

## 8. Changelog

Newest first. Every change to the project adds an entry here (rule in `CLAUDE.md`).

### 2026-10-15 — Save fix, settings menus, chapter thumbnails, background shading
- "Speichern …" in the menu editor did nothing: the capability lacked `dialog:allow-save` and
  the rejected `save()` was not caught. Added the permission, `.json` suffix, and visible
  success/error messages (`flash`) for save, load and file dialogs; Rust round-trip test.
- Custom layouts: menu role `settings` (VTSM, after the root menus), actions `setAudio {source}`
  / `setSubtitle {source|null}` (source = ffprobe stream index, mapped to the DVD stream in the
  build; only in a title's root/settings menus; validated against the selected tracks).
- Background `gradientOverlay` (`none`, `bottomDark`, `vignette`, `dim`) in
  `encoder::custom_menu_args` (gradient rendered once via `geq` + `loop`).
- Draft: chapter pages with moving thumbnails starting at each chapter (6 per page), an
  "Einstellungen" menu per title with several audio tracks or subtitles, `.gitattributes` (LF).
- Verified: `cargo check --workspace` clean, unit tests 97 engine + 9 GUI green,
  `npm run build` (tsc) clean, overlay filters run with real ffmpeg (manual test).
  Afterwards (see the next entry): full test suite, end-to-end settings build, editor UI.

### 2026-10-15 — Settings menus verified end to end; shading preview in the editor
- New integration test `settings_menu_chapter_thumbnails_and_shading`: clip with 2 audio
  tracks, an SRT subtitle and named chapters → layout with chapter video buttons (start 0/4/8 s),
  settings menu (`setAudio`/`setSubtitle`), all three shadings → real build; dvdauthor accepts
  `audio = 0/1; jump menu 3;`, `subtitle = 0/64; …`, settings entered via dispatcher code 102.
- Editor: background shading is now previewed on the stage (CSS gradients matching the ffmpeg
  filters).
- Files: `tests/custom_menu.rs`, `frontend/src/menuEditor.ts`, `frontend/src/styles.css`.
- Verified: full `cargo test --workspace` run by the user before this entry (97 engine unit,
  28 integration tests green); new test green (custom_menu.rs 4/4); `npm run build` clean;
  release GUI (separate target dir) with a 2-audio/1-subtitle/3-chapter clip: draft has
  chapter page with video buttons + settings menu, action panel "Tonspur wählen → Deutsch",
  dim/bottomDark previews; **"Speichern …" writes the JSON through the native dialog** (green
  "Gespeichert: …" message); `build-dvd --layout <saved file>` builds the DVD.

### 2026-10-15 — Git repository; real burn verified by the user
- `git init` (branch `main`, repo-local identity `malik <user@example.invalid>`), `.gitignore`
  (target, node_modules, dist, ffmpeg/ffprobe binaries > 100 MB, ISO images, work dirs,
  patch scripts, local Claude settings). Removed the stray `output_menu/` folder.
- The user reported a successful real burn on the Pioneer BDR-209M: IMAPI2 write, finalize,
  eject, read-back verification and burn ETA work (§5.2). Erase and DL layer break remain
  untested (§5.3).
- Files: `.gitignore` (new), `STATUS.md`; `output_menu/` deleted.
- Verified: `git status` lists only source, config, docs, icons and the small self-built tools.

### 2026-10-15 — Menu draft from the live project titles; stale layouts
- Reported: the editor showed placeholder-like data for a 4-title project. Cause: `draftLayout`
  already received the live titles, but a layout stored in localStorage from an earlier
  project (2 test videos) was reopened instead of a draft, and chapter buttons/video badges
  used generic "Kapitel N"/"Titel N" texts.
- `EditorTitle` now carries `index` (1-based disc title) and `chapterNames` (source chapter
  names, same sorted chapter list the build uses); `draftLayout` addresses titles by `index`
  and labels chapter buttons with the real names ("Kapitel N" only for unnamed chapters).
- The layout remembers which videos it was made for (`titlesKey`, stored as
  `convertx-menu-layout-titles`); older layouts without it count as foreign when they skip a
  current title or refer to a missing one. Then the menu tab warns, converting is blocked and
  the editor opens a fresh draft (old layout via "Rückgängig"). Video badges, chapter pickers
  and action summaries show real labels/chapter names; the background-video start field
  shows the effective default (10 %).
- Files: `frontend/src/{layout.ts,menuEditor.ts,main.ts}`.
- Verified: `npm run build` and `tsc --noEmit` clean; draft for 4 clips with 3/5/1/2 chapters
  (named and unnamed) → correct menus/buttons, `build-dvd --layout` with real dvdauthor OK;
  GUI with the 4 clips: stale warning, convert blocked, editor draft with real labels and
  chapter names, "Layout ist gültig (4 Menüs)". The stored test layout in the GUI was
  discarded and the mode reset to "ConvertX-Vorlage".

### 2026-10-15 — WYSIWYG menu editor (custom menu layouts)
- New engine module `src/layout.rs`: JSON layout model, validation (36 buttons, 128 commands,
  unrotated non-overlapping hitboxes, targets), navigation planning with titleset dispatchers
  and a VMGM router, rendering of layers/masks/spumux XML. `encoder::custom_menu_args` builds
  the overlay graph dynamically (blurred image/video backgrounds, positioned/rotated/faded
  clips). `author.rs`: `ButtonAction::Commands`, command-only PGCs, `MenuSpec::pre`,
  `DvdLayout::title_pre`. `dvd.rs`: `BuildOptions::custom_layout`, menu stage refactored into
  `MenuStage` (template or custom). CLI `build-dvd --layout`.
- GUI: `src-tauri/src/menu_editor.rs` (`check_layout`, `load_layout`, `save_layout`,
  `image_preview`), `title_frame`, `customLayout` in `start_conversion`; frontend
  `menuEditor.ts`, `layout.ts`, menu-tab mode switch in `main.ts`, editor styles.
- Files: `src/{layout.rs (new),author.rs,encoder.rs,menu.rs,dvd.rs,main.rs,lib.rs}`,
  `tests/custom_menu.rs` (new), `src-tauri/src/{main.rs,menu_editor.rs (new)}`,
  `frontend/src/{menuEditor.ts (new),layout.ts (new),main.ts,api.ts,styles.css}`.
- Verified: 132 tests green (new: 7 layout/graph unit tests, 1 GUI test, 3 integration tests
  with real tools); release GUI clicked through (open editor, drag, add text, apply); engine
  output inspected for WYSIWYG fidelity. Not verified: playback on a real DVD player.

### 2026-10-15 — STATUS.md upkeep rule
- Added `CLAUDE.md` with the rule to update STATUS.md after every change; added this changelog.
- Files: `CLAUDE.md` (new), `STATUS.md`.
- Verified: files reviewed; no code change.

### 2026-10-15 — STATUS.md created
- First full hand-over document (sections 1–7). Corrected stale module docs in
  `src/encoder.rs` (no more `.m2v`/`.ac3` intermediates) and `src/dvd.rs` (output layout).
- Verified: `cargo check --workspace` clean; test count 121 taken from the last full run.

### 2026-10-15 — No console windows, faster encoding, burn settings, ETA
- Every child process via `tools::tool_command`: `CREATE_NO_WINDOW` + priority, stdin closed,
  kill-on-drop; ffprobe in `probe.rs` too.
- Encoding: video + AC-3 straight into the DVD program stream (no mux pass, no elementary
  streams on disk); SRT extracted first, then `ffmpeg | spumux … > title.mpg` pipeline;
  `-threads 0`; optional GPU decoding (`hw_decode`, `--hw-decode`, probe cuda/d3d11va/qsv).
- Burn settings in `AppSettings` + settings page "Brennen": `burn_speed`, `verify_after_burn`,
  `keep_iso`, `delete_after_burn`, `quiet_finish`, `auto_erase_rewritable`,
  `default_volume_label`; DVD-RW erase via `IDiscFormat2Erase`; `open_folder` command.
- New `src/eta.rs` + `burner::BurnEta`; `Event::Eta`; overlay shows "Phase n/5" + remaining
  time; burn dialog shows per-speed duration and live ETA.
- Files: `src/tools.rs`, `src/probe.rs`, `src/encoder.rs`, `src/dvd.rs`, `src/eta.rs` (new),
  `src/config.rs`, `src/main.rs`, `src/burner/{mod,windows,unix}.rs`, `src-tauri/src/burn.rs`,
  `src-tauri/src/main.rs`, `frontend/{index.html,src/*.ts,src/styles.css}`, `tests/two_pass.rs`.
- Verified: 121 tests green; GUI conversion with 0 console windows; benchmark 17.2 s → 15.5 s
  (13.8 s with CUDA decode); ETA plausibility on a real 2-pass build. Burning untested.

### 2026-10-14 — Post-authoring disc burner
- `src/burner/`: volume label sanitiser, own UDF 1.02 + ISO 9660 image writer, DVD-9 layer
  break (cell starts, %16, L0 ≥ L1, padding + TT_SRPT patch), IMAPI2 burner (own IStream,
  verification, eject), growisofs/xorriso fallback; CLI `drives`, `make-iso`; GUI burn dialog
  with 20 s auto-start countdown; `convertx-gui <VIDEO_TS>` opens it directly.
- Verified: images mounted by Windows (byte-identical files), `isoinfo`/`udfinfo` in WSL,
  drive enumeration on the real Pioneer BDR-209M; no disc was burnt (by request).

### Before 2026-10-14 — Engine, CLI and GUI
- Template parser, bitrate calculator, menu renderer (still + motion, PAL/NTSC), dvdauthor/
  spumux authoring with full navigation and playback options, parallel encoding with 2-pass,
  cancellation, settings dialog, Tauri GUI, native dvdauthor/spumux build for Windows.
