# STATUS — convertx-rust

> Single source of truth for handing this project to another contributor (human or AI).
> State as of **2026-10-24** (incl. **Burn Studio milestones M1 + M2 + M3a + M3b + M4 (M4: `burn-studio` window, Tauri commands/events `burn_studio.rs`, shared operations layer `burner/studio.rs`, CLI `read-disc`/`verify-disc`/`build-iso`/`iso-label`/`ifo-layerbreak`/`drive-info`, `--burn-studio` start flag — nothing of it ever ran against a real disc; M3b: direct MMC write engine `mmcwrite.rs`, opt-in, never run on a real drive; MMC command layer with IMAPI-Ex and SPTI transports, drive info, standby/tray guards, `burn_source` with ring buffer, graph telemetry + `.ibg`; settings, sidecars, ISO reader, label patcher, layer-break inspector, generic UDF/ISO/Joliet tree writer, builder, sources, ring buffer, reader, verifier; plan not finished, §6.1)**, **Blu-ray menus from the menu editor (HDMV + IGS, VLC-verified)**, **NVENC for Blu-ray (verified on RTX 4070 SUPER, driver 617.42)**, **Blu-ray output**, menu editor, fast encoding, CLI `burn`, shared burn flow, IMAPI write events, Half D1/SIF + SP/MP/LP, bitmap subtitles, audit fixes). Everything marked ✅ was built **and** exercised (tests or real
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
disc. Since 2026-10-15 it also writes a **Blu-ray** (`BDMV` + `CERTIFICATE`, H.264 + AC-3) for
BD-25/BD-50 – menu-less autoplay, or (since 2026-10-17) with HDMV menus designed in the menu editor – and burns it. It consists of:

- a **Rust engine library** (`convertx_rust`, `src/`) that does probing, bitrate planning,
  menu rendering – from the *original ConvertX template INIs* **or** from a custom JSON layout
  designed in the built-in **WYSIWYG menu editor** – encoding, authoring, disc-image creation
  and burning;
- a **CLI** (`convertx-rust`, `src/main.rs`);
- a **desktop GUI** (`convertx-gui`, Tauri 2 + vanilla TypeScript, `src-tauri/` + `frontend/`),
  German UI.

**Scope reality check (important for the next contributor).** Two output formats exist:
**DVD-Video** (MPEG-2 + AC-3 + DVD subpictures, menus) and, since 2026-10-15, **Blu-ray**
(H.264 High@4.1 + AC-3 + PGS subtitles; **menu-less autoplay** (all videos in one playlist) or
**HDMV/IGS menus from the menu editor**; authored by tsMuxeR plus our own navigation files; see
[§2.8](#28-blu-ray-subsystem)). **Not implemented:** BD-J, Blu-ray pop-up menus, ConvertX templates on Blu-ray, LPCM/DTS/TrueHD output, HEVC/UHD, AVCHD on DVD,
a custom size for BD, multi-session. HEVC etc. are supported as *input* only. See
[§6](#6-roadmap--immediate-next-steps).

**Core philosophy (as actually practised):**

- **Clean room.** No ConvertX code or binaries are used; only its *template files* (INI + PNG
  assets in `C:\ProgramData\VSO\ConvertXToDVD\7\Templates`) are read, parsed by our own parser.
  The custom layout path (menu editor) needs no ConvertX files at all.
- **Few black boxes, and only proven ones.** Heavy codecs are delegated to the `ffmpeg` CLI;
  DVD IFO/VOB structure to `dvdauthor`, subpicture muxing to `spumux` (both self-built static
  Windows binaries), Blu-ray BDMV/m2ts muxing to `tsMuxeR` (official release). Everything else is
  our own Rust: template parser, menu compositor,
  subpicture masks, navigation/command generation, bitrate model, **UDF 1.02 + ISO 9660 image
  writer**, **UDF 2.50 (metadata partition) image writer for Blu-ray**, **dual-layer break calculation**, **IMAPI2 burner**. No mkisofs/genisoimage/libcdio.
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
├─ bin/                  dvdauthor.exe, spumux.exe (self-built, tracked), tsMuxeR.exe (+ licence, tracked),
│                        ffmpeg.exe, ffprobe.exe
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
  `-dc 9 -qmin 2 -mbd rd -trellis 1` (`-trellis 1` dropped in fast mode: `EncodeJob::fast`,
  `BuildOptions::fast_encode`, setting `fast_encode`, CLI `--fast`), VBV 1835 kbit, maxrate ≤ 9800 − audio, BT.601 colour
  tags, `-aspect 4:3|16:9`. Filter chain: `bwdif` (interlaced sources) → `fps` →
  `scale` (lanczos, BT.601, tv range) → `pad` (letter/pillarbox) → `setdar` → `format`.
- **Title resolution** (`author::Resolution`): Full D1 720×576/480, Half D1 352×576/480, SIF
  352×288/240 (setting "Auflösung", CLI `--resolution`); only the titles – menus stay Full D1,
  subpictures keep the full raster. Half D1/SIF titles are always 4:3 (wide sources letterboxed,
  one warning) – a conservative reading of the DVD-Video rule that 16:9 needs a 720/704 frame.
- **Encoding profiles** (`calculator::EncodingProfile`, setting "Profil", CLI
  `--encoding-profile`): ceiling of the video bitrate – Auto/SP 8500, MP 6000, LP 4000 kbps
  (`calculate_capped`); LP encodes a Full D1 project in Half D1 when the computed bitrate is
  below 3000 kbps (`dvd::effective_resolution`). The GUI's live estimate uses the profile too.
- **Audio:** AC-3 48 kHz, 192 kbps stereo / 448 kbps 5.1 (`ac3_settings`), up to 8 tracks.
  **LPCM, MP2, DTS are not produced.**
- **Mux:** done by ffmpeg itself in the encode run (`-f dvd -muxrate 10080k -packetsize 2048`).
  There is no separate mux pass and no `.m2v`/`.ac3` on disk any more.
- **Subtitles:** text subtitles (SRT/ASS/…) → SRT → `spumux` text rendering (font `arial.ttf`
  copied to `<work>/.spumux/`), chained via pipes: `ffmpeg | spumux -s0 | spumux -s1 > title.mpg`.
  **Bitmap subtitles** (`subpic.rs`, own decoders): PGS is extracted by ffmpeg as a raw `.sup`
  (in the same run as the SRTs), VobSub is read with `ffprobe -show_packets -show_data` (hex
  dump; times made relative to `format.start_time`; palette/size from the Matroska `.idx`
  extradata, else white/black/grey and the source's DVD raster). Each displayed picture is
  mapped from the subtitle canvas onto the picture area of the 720-wide frame (letterbox/crop
  aware), reduced to transparent + 3 colours (alpha < 128 → transparent, k-means), cropped and
  written as a palette PNG under `<work>/<stem>_s<n>/`; a spumux XML of `<spu image start end
  xoffset yoffset [force]>` entries goes through the same spumux pipeline, so DVD stream numbers
  follow the selection order of text and bitmap subtitles alike. DVB/XSUB/teletext bitmaps are
  still skipped with a warning.
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
- **Blu-ray (`BDMV`) authoring is a separate path** (`bd.rs`, `bd_author.rs`, tsMuxeR, see
  [§2.8](#28-blu-ray-subsystem)); `dvd::build_dvd` hands BD discs over to `bd::build_bd` before any
  DVD logic runs.

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
  Since 2026-10-17 the editor runs in its **own window** (label `menu-editor`, page `editor.html`,
  independent of the main window): `main.ts` opens/focuses it and answers `editor-ready` with
  `editor-init`; `editor-apply` carries the layout back (the editor stays open after "Übernehmen").
  Closing the main window destroys the editor (Rust `on_window_event`). Elements have an optional
  editor-only `group` (one level; groups of one are dissolved); the layer list shows the z-order.

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
  that aborts on cancel, **write progress from IMAPI's `DDiscFormat2DataEvents`** (own
  `#[implement(DDiscFormat2DataEvents)]` sink, vtable `Update` and `IDispatch::Invoke` DISPID
  0x200; action → phase via `write_update`: validating/formatting/initializing/calibrating →
  Formatting, writing → Writing with `(LastWrittenLba − StartLba) / SectorCount`,
  finalization/verifying → Finalizing; cancel calls `CancelWrite` from the event). Only
  connected on an MTA thread; until the first event (or without events) progress still comes
  from the data handed to the stream. Verification by raw reading `\\.\X:` and comparing
  with the image, eject.
- **Unix burner** (`burner/unix.rs`): drives from `/sys/block/sr*`, media via
  `xorriso -outdev … -list_speeds -toc`, burn with `growisofs -dvd-compat [-speed]
  [-use-the-force-luke=break:L0] -Z dev=iso` or `xorriso -as cdrecord -dao`, RW blanking
  with `xorriso -blank as_needed`, verification via the device node.
- **Speeds:** IMAPI reports sectors/s; 1x DVD = 1,385,000 B/s ≈ 676.27 sectors/s, 1x BD =
  4,495,500 B/s ≈ 2,195.07 sectors/s (the speed list stays in "x" and is read per medium). Default:
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

### 2.8 Blu-ray subsystem

Choosing **BD-25 / BD-50** (`DiscType::Bd25/Bd50`, JSON `"bd25"/"bd50"`, targets 22,900 / 45,800
MiB, mux overhead 6 %) switches the whole pipeline; the DVD path is untouched. Without a custom layout the disc is
**menu-less**: it autoplays one playlist made of all videos (this section's diagram). With a layout
from the menu editor see **Blu-ray menus** at the end of this section.

```text
 inputs ─ probe + select_streams (as for DVD; only SRT-like text and PGS subtitles survive) ─
 harmonised tracks (audio k exists if any title has it; missing → silent AC-3 192k; subtitle
 track k = kind/language of the first title that has it, else an invisible SRT / empty PGS)
   │  one project frame rate (bd::bd_frame_rate)
   ▼  bitrate_plan (menu 0 MB, BD limits 4000–35,000 kbps, total ≤ 48,000)
 [2/5] per title (parallel): subtitle extraction (SRT / PGS .sup) + placeholders → ffmpeg
        libx264 (or h264_nvenc, see below) High@4.1 -bluray-compat + AC-3 (apad, -t) → <work>/title_NN.mkv
 [3/5] bd.meta (bd_author) → tsMuxeR (concatenation "a.mkv"+"b.mkv", --custom-chapters at every
        chapter of every title with absolute times) → <work>/BD/BDMV → <project>/BDMV, plus an
        empty <project>/CERTIFICATE/BACKUP (tsMuxeR 2.7 writes no CERTIFICATE folder)
 burn: burner::burn_video_ts → burner::udf250 (own UDF 2.50 writer, no ISO 9660 bridge,
        no layer break) → IMAPI2 (BD-R / BD-RE)
```

- **Frame rate** (`bd::bd_frame_rate`): all sources film (23.976 / 24 ± 0.01) → keep it (mixed
  film rates → 24000/1001), progressive. Otherwise the TV rate: 1080 → 25 / 29.97 with x264
  `fake-interlaced=1:pic-struct=1` (flagged 1080i, because 1080p25/29.97 do not exist on a BD),
  720 → 50 / 59.94 progressive (frames doubled by the `fps` filter). One rate for the whole
  project, because tsMuxeR's concatenation needs identical SPS. GOP = 1 s.
- **Picture:** always 16:9, square pixels, BT.709; 4:3 sources are pillarboxed; `Resolution::Hd1080`
  / `Hd720` (settings `bd_resolution`, CLI `--resolution hd1080|hd720`). A DVD with an HD resolution
  or a BD with a DVD resolution fails with a German message before any work is done.
- **Encoder choice** (`bd.rs`, `encoder::BdEncoder`): with `BuildOptions::bd_hw_encode` (setting
  `bd_hw_encode`, **default on**; CLI `--no-hw-encode` turns it off) the build probes
  `Toolchain::probe_nvenc` once (10 test frames, the BD options, 30 s timeout) and encodes with
  `h264_nvenc` (`-preset p5`/`p3` fast, `-tune hq`, High@4.1, `-bluray-compat 1 -aud 1 -slices 4
  -rc vbr -forced-idr 1 -strict_gop 1 -spatial-aq 1 -rc-lookahead 32`, `-bf 3`, same
  maxrate/bufsize/GOP). Otherwise x264, with a warning carrying the reason (`nvenc_failure_reason`:
  driver too old, no NVENC GPU, or the last log line). **1080i rates (`fake_interlaced`) always use
  x264** (NVENC has no `fake-interlaced`; Step explains it). 2-pass with NVENC = `-multipass
  fullres` in the single run (no analysis run, no passlog; `BdEncodeJob::x264_two_pass()`; the ETA
  counts 1 pass). Parallel NVENC encodes are capped at `NVENC_MAX_JOBS = 4` (measured 2026-10-17: 4 titles take 45–47 s at 1 or 4 jobs, so the cap is not a limit; value kept).
  DVD encoding is untouched (MPEG-2 stays software). Needs NVIDIA driver ≥ 610 for ffmpeg 9.0.1.
- **Encoder profile ceilings** on BD: Auto/SP 35,000, MP 20,000, LP 12,000 kbps
  (`EncodingProfile::max_video_kbps_for(disc)`); the LP → Half D1 switch never applies.
- **`--single-video` / `bd_single_video`:** more than one input is rejected before any work.
- **Tools:** `Toolchain::tsmuxer` is optional (DVD builds do not need it); a BD build without it
  fails naming `bin/tsMuxeR.exe`.
- **UDF 2.50 writer** (`burner/udf250.rs`): VRS `BEA01/NSR03/TEA01`, descriptor tag version 3,
  domain identifier `*OSTA UDF Compliant` revision 0x0250, LVD with a type 1 (physical) and a type 2
  **metadata partition map** (`*UDF Metadata Partition`, duplicate-metadata flag, allocation and
  alignment unit 32). Physical partition at sector 512: Metadata File EFE (type 250, block 0) and
  Mirror File EFE (type 251, block 1), copy 1 at block 32, file data (each file on a 32-block
  boundary, long_ads with partition reference 0, extents < 1 GiB), copy 2 behind the data. Everything
  else (FSD, every directory/file EFE, FIDs) lives in the metadata partition (reference 1). Generic
  tree of any depth (sorted names, empty directories kept); only `BDMV` and `CERTIFICATE` of the
  project folder go onto the disc. Planned first (`plan_bd_image`), then written
  (`write_bd_image`), cancellable, via `<name>.iso.part`.
- **Burner:** `MediaType::{Bd25, Bd50}` (`bd_media_type(total sectors)`: > 12,219,392 → BD-50),
  BD-RE counts as rewritable and is overwritten like DVD+RW; `DiscContent` decides which kind of
  media a folder needs (`media_problem`); BD estimates use 1x = 4,495,500 B/s.
- **Blu-ray menus (HDMV / IGS, since 2026-10-17)** – `BuildOptions::custom_layout` on a BD disc
  (CLI `--layout`, GUI "Eigenes Menü (Editor)" on a BD disc):
  - *Layout:* the same `CustomLayout` JSON, designed on a **16:9 canvas (1920×1080)** with square
    pixels (`layout::validate_bd(layout, titles, MenuFrame)`: 16:9 canvas, buttons axis-aligned and
    non-overlapping, ≤ 255 buttons per menu, any number of colours; no VM-command limit).
    DVD and BD layouts are separate (frontend keeps one slot per medium). `MenuFrame {width,
    height, pixel_aspect}` (in `author.rs`, `From<TvStandard>`) replaced the TV-only parameters of the
    shared render helpers; DVD output is unchanged (all earlier tests green).
  - *Disc structure:* playlist/clip `i` = menu `i` (layout order), then one playlist per title
    (`bd_menu::title_playlist`). Movie objects: 0 First Play, 1 Top Menu, `2+i` menu `i`, then one
    per title; `index.bdmv` lists the title objects. `hdmv.rs` writes `index.bdmv` / `MovieObject.bdmv`
    (layout checked against tsMuxeR's own files) and overwrites tsMuxeR's after all runs.
  - *Menus:* background, static elements and video layers are rendered into the video (ffmpeg
    graph shared with the DVD menus, `encoder::bd_custom_menu_args`: x264 High@4.1 8 Mbit/s + AC-3);
    buttons are an **IGS** stream (`igs.rs`: ICS with one BOG per button, neighbours from
    `author::compute_navigation`, PDS with real alpha (≤ 255 colours, 16 alpha steps), ODS with RLE
    and fragmentation; selected + activated object per button cropped to the button rect, *normal*
    = nothing (the normal look is in the video); object budget 16 MiB). A **still** menu is a 1 s clip
    whose PlayItem has `still_mode` = infinite and `default_selected_button` = first; a **motion**
    menu (video background/elements or sound) loops: its object is `SetButtonPage(1); PlayPL(menu);
    GOTO 1` and the ICS has `default_selected_button` = 0xFFFF so the selection (PSR10) survives each loop.
  - *Muxing trick (`bd_patch.rs`):* tsMuxeR only copies PGS segment types and silently drops
    unknown ones (zero packets on the PID). So the IGS goes in as `S_HDMV/PGS` with the ICS labelled
    as a PCS (0x16); after the run `patch_menu` rewrites TS PID 0x1200 → 0x1400 in every packet,
    segment type 0x16 → 0x18 at the start of the ICS PES, the PMT stream type 0x90 → 0x91 + PID (+ CRC32),
    the CLPI program-info entry, and the MPLS STN entry (PG count 1→0, IG count 0→1, PID, coding type);
    still menus get `still_mode` = 1. Menus are muxed one run each (`--mplsOffset=N --m2tsOffset=N`,
    offsets accumulate in the shared `BD` folder), then the titles (one run each).
  - *Navigation (`bd_menu::plan`):* `GPR1` chapter mark the next title object starts at, `GPR2`
    play-all flag, `GPR6` scratch. Buttons: play title = `GPR1=0; GPR2=0; JumpObject(title)`, chapter
    k = `GPR1=k-1`, play all = `GPR2=1`, menu = `JumpObject(menu)`. A title object is
    `PlayPL_at_mark(pl, GPR1); GPR1=0;` then the DVD-equivalent of `dispatcher_commands`
    (sequential / play-all next, loop, return to the first title menu that plays it, or to the title's
    own menu). Audio button = `SetStream` with the audio number (the display flag is rewritten by every
    `SetStream`, so it is re-derived from PSR2 first); subtitle button = `SetStream(0xC000|n)`,
    off = `SetStream(0)`. Numbers are the position in the harmonised disc track list (1-based). First Play:
    first title menu, or title 1 with auto-start. "Menu" / Top Menu key → first title menu. Root and
    settings menus are ordinary menus (no popup menu).
  - *Sizes:* menu clips count in the bitrate plan with their real muxed size; the GUI estimates
    1.1 MB per clip second (`bdMenuSecs`).
- **Deviations from the plan file** (decided while implementing): the Metadata (Mirror) File
  Location fields of the partition map point at the two EFEs (blocks 0/1 of the physical
  partition) – only the *bitmap* location is 0xFFFFFFFF – because readers need them; `CERTIFICATE`
  is created by us; the BD capacities live in `udf250` (`BD25_SECTORS`, `BD50_SECTORS`), not as
  `Capacities` constants (`layerbreak.rs` is unchanged).

---

## 3. Implemented modules & current status

Line counts as of this writing (total ≈ 23,900 lines incl. tests and frontend).

### 3.1 Engine (`src/`)

| File | Lines | Responsibility | Status / completed | Pending |
|---|---:|---|---|---|
| `lib.rs` | 21 | Module list | ✅ | — |
| `template.rs` | 621 | ConvertX template INI parser (pages, buttons, backgrounds, colour schemes, texts) | ✅ all "Open Style" templates parse | Other template styles only smoke-tested via the folder listing |
| `calculator.rs` | 437 | Bitrate model: `video = (target·8192 − audio·t − overhead)/t`, clamp 1000–8500 kbps and to 9800 − audio (per disc via `DiscType::limits()`: BD 4000–35,000 and 48,000 − audio); `calculate_capped` (profile ceiling); `EncodingProfile {Auto, Sp, Mp, Lp}` with `max_video_kbps` (8500/8500/6000/4000) and `max_video_kbps_for(disc)` (BD 35,000/35,000/20,000/12,000), `LP_HALF_D1_BELOW_KBPS` = 3000; `DiscType {Dvd5 4300 MB, Dvd9 7950 MB, Bd25 22,900 MB, Bd50 45,800 MB, Custom}` (`is_bluray`, `limits() -> DiscLimits`, mux overhead 2 % DVD / 6 % BD); `Quality` good > 2500 / medium / poor < 1200 (`Quality::for_disc`: BD 15,000 / 8000) | ✅ (BD limits, ceilings, quality unit-tested) | SP behaves like Auto (both fill the disc up to 8500 kbps) |
| `subpic.rs` | 1009 | **Bitmap subtitles:** PGS decoder (`decode_pgs`: PCS/WDS/PDS/ODS/END, epochs, fragmented objects, crop, forced flag, BT.709/601 palette, `decode_pgs_rle`), VobSub decoder (`decode_vobsub`: SPU control sequences, 2-bit interlaced RLE, `.idx` palette/size, no-palette fallback), ffprobe hex-dump parsing (`vobsub_probe_args`, `parse_vobsub_probe`, `parse_hexdump`), `Target` (canvas → frame mapping incl. letterbox and cropped videos), `quantize` (transparent + 3 colours, crop), `render` (palette PNGs + spumux XML, overlap trimming, minimum duration), `empty_pgs` (a `.sup` with one empty display set: placeholder PGS track for Blu-ray) | ✅ unit-tested with handcrafted segments/SPUs; ✅ real VobSub (ConvertX DVD from D:) and PGS fixtures end to end | PGS only as raw stream copy (no PGS in AVI/MP4 tested); semi-transparency is reduced to on/off |
| `probe.rs` | 422 | ffprobe JSON → `MediaInfo` (video, audio, subtitles with kind text/bitmap, chapters), ISO 639 helpers | ✅ | — |
| `tools.rs` | 591 | Tool lookup (`bin/` next to exe → `./bin` → `PATH`, candidates verified with `-version`; tolerant of a stray `"` in PATH), `tool_command` (no window, priority, kill-on-drop), **`blocking_tool_command`** (std variant: no window, stdin closed – for the Unix burner and `open_folder`), `ffmpeg`/`ffmpeg_with` (telemetry), `ffprobe_output`, `ffmpeg_spumux_pipeline`, `spumux`, `dvdauthor`, `probe_hwaccel`, **`probe_nvenc`** + `nvenc_failure_reason`, optional **`tsmuxer`** (found like the others, verified by running it without arguments and looking for its banner; `Toolchain::tsmuxer(dir, meta, out)`; `None` = DVD builds still work) | ✅ | — |
| `encoder.rs` | 1517 | ffmpeg argument builders (custom menus: `menu_pixels` – square pixels, then the 4:3 menu pixel shape): `EncodeJob` (filters, codec, 2-pass, **fast mode without trellis**, **title resolution**, direct DVD PS output, `subtitles` in DVD order: SRT + PGS `.sup` extraction, hwaccel), `active_size(source, aspect, frame)`, `video_filter(…, resolution)`, `run_encode` (+ `bitmap_subtitle_xml`: decode/render on blocking threads), thumbnails, **`BdEncodeJob`** (H.264 `-bluray-compat` High@4.1 + AC-3 into Matroska: `bd_video_filter` bwdif → fps → lanczos/BT.709 scale → pad → `setsar=1`, x264 `slices=4:b-pyramid=strict:nal-hrd=vbr:aud=1:open-gop=0:weightp=1`, `fake-interlaced` only for 1080 TV rates, 1 s GOP, silent AC-3 filler tracks, `apad` + `-t`, 2-pass, fast = `veryfast`) with **`BdEncoder::{X264,Nvenc}`** (`nvenc_args`, `nvenc_bd_options`; no analysis run for NVENC) and `run_bd_encode`, `extract_subtitle_args` / `subtitle_file` (SRT + PGS extraction shared with `EncodeJob`), `motion_menu_args`, `menu_still_args`, **`custom_menu_args`** (dynamic layer graph: `MenuBackground`, `MenuLayer`, `Fit`, `CustomMenuMedia`; the graph is `custom_menu_graph(frame, rate, …)`, `menu_pixels_for(pixel_aspect)`) and **`bd_custom_menu_args`** (the same graph on a square-pixel HD frame, x264 High@4.1 8 Mbit/s + AC-3 into Matroska), `spumux_textsub_xml`, `MenuAudio` | ✅ (fast-mode, Half D1/SIF and all BD argument builders unit-tested; NVENC args unit-tested, ✅ real NVENC build (tsMuxeR, libbluray); BD encodes run in `tests/bluray.rs`; bitmap path via `tests/bitmap_subs.rs`) | The ~40 % speed-up of fast mode is the roadmap estimate, not measured here |
| `menu.rs` | 1369 | Page rendering (backgrounds, texts, masks, motion layers/slots), pagination, parallel rendering, `RenderOptions`, `MenuPageFiles` (+ `custom` media); crate helpers `text_coverage`, `blit`, `blend_coverage` for custom layouts | ✅ PAL + NTSC, still + motion | Half-D1/SIF menus n/a |
| `layout.rs` | 1699 | **Custom menu layouts** (menu editor JSON): model, `validate` (DVD) / **`validate_bd`** (16:9, ≤ 255 buttons, no colour/VM limits; both through `validate_impl`), `plan_navigation` (DVD dispatchers, router, register scheme), `build_custom_menus` (layers, masks, spumux XML, `MenuSpec`s); render helpers (`frame_box`, `frame_font_size`, `button_rect`, `element_text_lines`) take `impl Into<MenuFrame>`, `bd_button_rect`, `fit_image`/`rotate`/`clip_of` are `pub(crate)` | ✅ unit tests + real builds (PAL/NTSC) accepted by dvdauthor | Navigation not yet tried on a real DVD player/VM; text lines are cut with "…" (no word wrap) |
| `author.rs` | 1392 | `TvStandard` (+ `menu_pixel_aspect`), **`MenuFrame`** (size + pixel aspect of the frame a menu is drawn on), **`Resolution`** (`frame_size`, `label`, `allows_wide`, `is_hd`; `Hd1080` 1920×1080 / `Hd720` 1280×720 for Blu-ray), `PlaybackOptions`, `MenuLinks`, `ButtonAction` (incl. `Commands` for ready-made command strings) → DVD VM commands, dispatcher PGCs, command-only menu PGCs (`MenuSpec::commands`), per-menu `pre`, `DvdLayout::title_pre`, `dvdauthor_xml`, `spumux_xml`, `TitleSpec`, limits | ✅ verified with real dvdauthor | No seamless-branching/angles |
| `dvd.rs` | 1862 | Orchestrator `build_dvd`: planning, stream selection (`select_streams`: user choice / language filter / preferred language first / DVD limits / unsupported bitmaps skipped; unit-tested), menus (template path **or** `BuildOptions::custom_layout` → `MenuStage`), thumbnails, menu encoding, bitrate (`bitrate_plan(…, profile)`), `effective_resolution` (LP → Half D1; reduced sizes force 4:3), title encoding (parallel, telemetry), dvdauthor, project folder, work-dir handling (`pub const WORK_DIR` = dir name and marker file, also used by the GUI), cancel, `Event` stream (`phase`, `step`, `progress`, `warning`, `eta`), `BuildOptions` (incl. `resolution`, `encoding_profile`), `BuildReport` (incl. `resolution`), `bd_single_video`; `build_dvd` hands BD discs to `bd::build_bd` and rejects HD resolutions for a DVD; helpers shared with `bd.rs` (`select_streams`, `run_limited`, `job`, `cancellable`, …) are `pub(crate)` | ✅ | Pipeline order is Menus → Videos (bitrate needs the real menu size) |
| `bd.rs` | 830 | **Blu-ray orchestrator** `build_bd` (checks before any work → probe/select → harmonised tracks → one frame rate → menu stage (`build_menus`: validate_bd → `bd_menu::plan` → render → ffmpeg encode of every menu (parallel) → one tsMuxeR run per menu + `patch_menu`; real size enters the bitrate) or "no menu" → bitrate with BD limits → NVENC probe/choice (`NVENC_MAX_JOBS`) → parallel `encode_bd_title` (subtitle files / placeholders, `BdEncodeJob`, real duration via ffprobe) → `bd.meta` (menu-less) **or** one tsMuxeR run per title with `--mplsOffset/--m2tsOffset` + `write_navigation` (own `index.bdmv` / `MovieObject.bdmv` into `BDMV` and `BACKUP`) → project folder `BDMV` + `CERTIFICATE/BACKUP`), `BdRate`, `bd_frame_rate`, `harmonise` | ✅ unit-tested (every frame-rate branch, harmonising); ✅ real builds in `tests/bluray.rs` (menu-less and with a 3-menu layout); ✅ played in VLC (§5.2) | One frame rate per disc; DVB/XSUB/VobSub subtitles skipped; menus: HDMV only |
| `bd_menu.rs` | 704 | **Blu-ray menus:** `traits` (moving / has buttons per menu), `plan` → `BdNav` (movie objects, title objects, per-button HDMV commands; register scheme and playback options in §2.8), `render_pages` → `BdPage` (PNG layers, `CustomMenuMedia`, audio, `IgMenu`: button rects, highlight images → shared palette with alpha → RLE objects, neighbours, 16 MiB budget check), `frame_rate_code` | ✅ unit-tested (object numbers, every button action, still/motion objects, every playback option, IG objects of a rendered page); ✅ VLC (§5.2) | Standalone players never tried; no popup menu / BD-J; sounds on buttons, animations, pages > 1 not used |
| `igs.rs` | 293 | Interactive Graphics segment writer: `build_sup` (ICS as PCS-labelled segment for tsMuxeR, PDS, fragmented ODS, END), `rle_encode` (inverse of `subpic::decode_pgs_rle`), `rgb_to_ycrcb` (BT.709 limited) | ✅ unit-tested (RLE round trip, 14-bit runs, segment/field lengths, fragmentation); ✅ rendered by libbluray in VLC | One page, one BOG per button, no effects/sounds |
| `hdmv.rs` | 208 | HDMV 12-byte command encoding (branch/play/jump/set/compare/`SetStream`/`SetButtonPage`, operands imm/GPR/PSR) and `index_bdmv` / `movie_object_bdmv` writers | ✅ unit-tested against bytes from tsMuxeR's own files; ✅ libbluray (VLC) parsed index + objects and executed every command used | Layouts compared with tsMuxeR's output and libbluray's source, not with a spec document |
| `bd_patch.rs` | 254 | PGS→IG patch of a tsMuxeR menu clip: `patch_m2ts` (PID, ICS type, PMT type/PID + CRC32), `patch_clpi`, `patch_mpls` (STN counts/PID/coding type, `still_mode`), `patch_menu` (clip + BACKUP copies), `crc32_mpeg` | ✅ unit-tested (CRC check value, byte diffs of a real tsMuxeR playlist, synthetic TS/CLPI); ✅ real builds (ffprobe sees `id=0x1400`, ICS/PDS/ODS/END in the PES) | Depends on tsMuxeR 2.7.0's layout (one PES per segment, PMT on PID 0x100, one PlayItem per playlist) |
| `bd_author.rs` | 252 | `BdProject` → tsMuxeR `bd.meta` (`meta_text`, per-title `title_meta_text(i, offset)`, `menu_meta_text(mkv, sup, offset)`: MUXOPT, V/A/S lines with `+` concatenation, `track=N`, languages as ISO 639-2/T, subtitle font parameters scaled to the frame), absolute chapter marks (≤ 999), `EMPTY_SRT` | ✅ unit-tested; ✅ accepted by tsMuxeR 2.7.0 | Text subtitles use tsMuxeR's own renderer (Arial 65 px at 1080p) |
| `eta.rs` | 326 | `JobPhase` (5 phases), `Eta` event payload, `FrameEta`, throughput/burn formulas (`Medium {Dvd, Bd}`: BD 1x = 4,495,500 B/s, 6x verify read; `burn_secs` / `default_speed_x` / `image_and_burn_secs` take it), `format_remaining` | ✅ unit-tested; plausibility-checked on real builds | Early estimate is pessimistic during ffmpeg start-up |
| `config.rs` | 1001 | `AppSettings` JSON (`%APPDATA%\convertx-rust\settings.json`), `sanitized()`, `apply_to(BuildOptions)` (incl. `dvd_resolution` / `bd_resolution` → `Resolution` depending on `opts.disc` (set the disc first), `bd_single_video`, `encoding_profile` → `profile()`); menu, TV, encoding (incl. `fast_encode`, **`bd_hw_encode`** default true), chapters, languages, **burn** settings; `max_concurrent_encodes` defaults to `dvd::default_jobs()` (half the logical CPUs); **`BurnStudioSettings`** (`AppSettings.burn_studio`: sections general/read/build/write/verify/graph/io + `auto_speed` rules; string choices like the rest of the file, unknown choice → default, numbers clamped in `sanitized()`) | ✅ backward compatible (missing fields → defaults, also for `burn_studio` and its sections) | `UNSUPPORTED_CHOICES` is empty now; the Burn Studio fields are read by `burner/studio.rs`, `mmcwrite::DirectOptions` and the settings dialog; options the engine ignores are listed in the `burner/studio.rs` row and shown disabled in the UI |
| `main.rs` | 844 | CLI (**M4:** subcommands `read-disc`, `verify-disc`, `build-iso`, `iso-label`, `ifo-layerbreak`, `drive-info` live in `studio_cli.rs`): `template`, `bitrate`, `render-menu`, `build-dvd` (`--disc dvd5\|dvd9\|bd25\|bd50`, `--template` **or** `--layout <JSON>` (DVD, or Blu-ray menus), `--two-pass --fast --single-video --resolution full-d1\|half-d1\|sif\|hd1080\|hd720` (default Full D1 / 1080p)` --encoding-profile auto\|sp\|mp\|lp --motion-menu --motion-duration --motion-silent --menu-audio --tv pal\|ntsc --project-name --jobs --keep-work --hw-decode --no-hw-encode` …), `drives`, `make-iso`, **`burn`** (VIDEO_TS, BDMV or project folder; `--drive --speed --label --no-verify --erase --no-eject --keep-iso [DATEI]`, Ctrl+C cancels, progress + remaining time, exit 130 on cancel; the flow itself is `burner::burn_video_ts`) | ✅ except `burn`: ✅ argument errors, missing VIDEO_TS and unknown drive (automated, `tests/burn_flow.rs`); 🟡 media checks and a real write via the CLI never run | `build-dvd` has no Ctrl+C handling (process just dies) |
| `burner/mod.rs` | 822 | **M4:** `pub mod studio`. **M3b:** `pub mod mmcwrite`. **M3a:** `BurnOptions` gained `ring_mib` / `recovery_low` / `recovery_high` / `prevent_standby` / `lock_tray` / `telemetry` (`Telemetry`, `TelemetryHook`), trait method `burn_source(drive, Box<dyn SectorSource>, …)` (default: unsupported; only IMAPI implements it), `verify_against_source`. Types (`DriveInfo`, `MediaType`, `WriteSpeed`, `MediaInfo`, `BurnPhase`, `BurnProgress`, `BurnOptions`, `BurnError` incl. `Image`), trait `OpticalBurner`, `system_burner()`, speed helpers, `BurnEta`, `create_disc_image`, `verify_against_image` (now a thin wrapper around `verifier::verify`: sequential device, no retries, stops at the first difference), `media_problem(media, &DiscContent)`, `DiscContent` (`bluray`, sectors, `dual_layer`, `of_folder`), `is_bdmv_folder`, `bd_media_type`, `MediaType::{Bd25, Bd50}`, speed helpers per medium, `default_iso_path`, **`burn_video_ts`** (`BurnJob` → image (DVD: UDF 1.02 + layer break hand-over; BD: `udf250`, no layer break), burn on a scoped progress thread, remaining time per `BurnUpdate`, image removal unless kept after success – a file at the ISO path is only removed when this run wrote it (an existing file survives a failed/cancelled image phase), cancel → `Cancelled`; used by GUI and CLI) | ✅ unit-tested (incl. media/content cross cases, BD speeds); `burn_video_ts` with a fake burner for DVD and BD content (`tests/burn_flow.rs`) | — |
| `burner/label.rs` | 58 | Volume label sanitiser | ✅ | — |
| `burner/sidecar.rs` | 307 | **Burn Studio:** `.dvd` (`LayerBreak=` + image name; tolerant parser), `.cue` (one BINARY `MODE1/2048` or `MODE1/2352` track, MSF parsing, audio/multi-track rejected with German errors), `.mds` writer/parser (single DVD session, MODE1/2048 track, physical-format structure with the layer-0 end) | ✅ unit-tested (round trips, CRLF/LF, MSF, rejections) | 🟡 **`.mds` compatibility with Alcohol/Daemon Tools is unverified**: the layout was written from memory of the libmirage notes (offsets, track mode 0xAB, structure block) and only round-trips through our own parser |
| `burner/isoread.rs` | 328 | **Burn Studio:** ISO 9660 reader over any `Read + Seek` (`IsoImage::open/open_joliet/list/find/read_file/extent_map`, `path_at`; multi-extent files merged, case-insensitive paths) | ✅ unit-tested on images from our writer + a hand-made multi-extent directory | Joliet only by name decoding (no Joliet-specific checks); UDF-only images (our BD images) are rejected |
| `burner/tools.rs` | 575 | **Burn Studio** (not the process helper `crate::tools`): `read_label`, `patch_label` (in place: ISO PVD 40/190, Joliet SVD, UDF PVD/IUVD/LVD in main + reserve VDS, FSD incl. UDF 2.50 metadata file + mirror; CRC + tag checksum fixed), `ifo_candidates` / `ifo_candidates_iso` (cell walk through VTS_PGCITI → PGC → cell playback table, seamless/interleaved flags, 16-sector alignment with padding, non-seamless preference, `Candidate` DTO) | ✅ unit-tested: patched DVD image **byte-identical** to a fresh one (also shrinking), same for a UDF 2.50 image, Joliet descriptor on a hand-made image, candidates on a synthetic DVD (rules, ordering, interleaved filter, folder == iso result) | Candidates are not tried on a real DVD yet; DVD text label is not read; the `write_dvd_file`/`write_mds_file`/`write_cue_file` wrappers of the plan are not separate — use `sidecar::write_*` |
| `burner/builder.rs` | 789 | **Burn Studio "build image":** `detect` (DVD-Video / Blu-ray / data), `resolve_content` (setting `prompts` ask/always/never → `NeedsAnswer`), `collect` + filters (`skip_reason`: hidden/system/reparse/in use; skipped list with reasons), `plan_build` → `BuildPlan` (DVD: `plan_disc_with` for automatic or user layer break, `fix_title_sectors` rewrites TT_SRPT in IFO + BUP to the planned layout, ECC-block sharing of IFO/BUP only *warned*; Blu-ray: `BDMV` + `CERTIFICATE` (empty dirs when `create_certificate`), `reset_cpi` transform on `STREAM/*.m2ts` (top 2 bits of every 192-byte packet), disc name from `bdmt_eng.xml`; data: ISO+Joliet+UDF 1.02, UDF 1.02, or UDF 2.50), `build_to_file` (split, MD5 while writing, `.dvd`/`.mds`/`.cue` next to it, `.part` files removed on failure/cancel), `stream_image` (on-the-fly image into a ring buffer = `StreamSource`) | ✅ unit-tested (detection, prompts, collection with destinations, filters, data image + MD5 + dedupe, DVD layer break incl. forced break and refusal, `AUDIO_TS` off, TT_SRPT fix idempotent, BD CPI reset byte-exact + certificate + disc name, split/layout/cancel, streamed == written bytes, stopping the stream stops the builder); ✅ data tree image mounted by Windows | `ifo_bup_ecc_padding` (separating IFO/BUP ECC blocks) and `read_dvd_text` (label from DVD text) are **not implemented** (settings stored only); the `.clpi` CPI flag is not touched; hidden/system/reparse filters never saw a real hidden file (pure-function test only) |
| `burner/source.rs` | 588 | `SectorSource` trait (2048-byte sectors, random access; **M3a:** `rewindable()`, `ring_stats()`), `SourceReader` (source as a byte stream), `spawn_ring_feed` (producer thread → ring; hands the source back when done, cancels the ring when the source fails) with `FileSource` (image/device, zero-padded tail), `ZeroSource` + `discovery(media, requested)` (blank writable disc required, size ≤ free capacity), `Bin2352Source` (drops the 16 header + 288 EDC/ECC bytes), `ReadSource` (sequential `Read`), `StreamSource` (producer thread → ring buffer; sequential; `finish()` returns the producer's result), test `FakeDisc` (fails N times / always, corrupts) | ✅ unit-tested | Device handles are not implemented yet (M3a) |
| `burner/ringbuf.rs` | 338 | Bounded byte ring (`ring(capacity, low%, high%)` → `RingWriter`/`RingReader`/`RingStats`): blocking producer/consumer, **buffer recovery** (consumer waits for the high mark after dropping below the low mark; initial prebuffering), fill percent, recovery count, `cancel()` | ✅ state machine unit-tested with scripted pushes/pulls, wrap-around, 5 MB through two threads in order, cancel and dropped reader unblock the other side | Not yet wired into the IMAPI stream (M3a) |
| `burner/reader.rs` | 434 | Disc → image: `read_disc(source, out, &ReadOptions, cancel, progress)` with software retries (chunk, then sector by sector), zero-fill + list of bad sectors when ignoring errors, MD5, `SplitWriter` (`name_partN.iso`, 2 GiB / just below 4 GiB, `.part` files committed or deleted), `write_layout` (`auto`/`none`/`dvd`/`mds`/`cue`) | ✅ unit-tested with `FakeDisc` (full read + MD5, flaky sectors, unreadable sectors stop or zero-fill, cancel leaves nothing, split parts, layout files) | Hardware retries (`hw_retries`) are a drive setting (M3a); CSS/AACS not handled by design |
| `burner/verifier.rs` | 236 | `verify(disc, image, &VerifyOptions, …)` (sector compare, retries, MD5 of the disc, miscompare list capped at 1000 with an exact count, unreadable sectors, `stop_at_first`), `read_check` (readability only), `files_of` (sector → file via `isoread::extent_map`) | ✅ unit-tested with `FakeDisc` (identical, miscompares + unreadable, ignore flag, flaky retry, cap, stop at first, cancel, image error, file mapping) | Not connected to a real drive yet |
| `burner/spti.rs` | 1078 | **M3b fix:** `set_streaming()` had type and parameter-list length in the wrong bytes (length now at bytes 9–10, type at 8; the old test asserted the wrong layout). **Burn Studio M3a, MMC layer:** CDB builders for every command of the plan table (TEST UNIT READY … SEND DISC STRUCTURE, `streaming_descriptor`, `format_list`), `Sense` (fixed + descriptor format, German `describe()`, `recoverable()`), `trait ScsiTransport { exec(cdb, DataDir, buf, timeout) }` with `ScsiResult`, helpers `run` / `query`; transports **`ImapiExTransport`** (`IDiscRecorder2Ex`, no admin; exclusive access helpers) and **`SptiTransport`** (`IOCTL_SCSI_PASS_THROUGH_DIRECT` on `\\.\X:`, page-aligned buffer, sense buffer); parsers (INQUIRY, READ CAPACITY, GET CONFIGURATION + profile list, READ DISC INFORMATION, DVD physical format incl. book type and layer break, ADIP media ID, BD disc information media ID, READ BUFFER CAPACITY, GET PERFORMANCE write speeds); `drive_info(&dyn ScsiTransport) -> DriveDetails` (read-only) | ✅ every CDB byte by byte, sense, parsers on synthetic buffers, `drive_info` against a scripted fake drive (DVD+R DL with disc, empty drive; asserts only read opcodes are sent); ✅ **host probe on the real Pioneer BDR-209M** (no disc inserted): INQUIRY, 21 profiles, "keine Disc" through IMAPI-Ex **and** through SPTI (opened without elevation) | 🟡 everything that needs a disc: media ID offsets (ADIP 19/27/30, BD DI 100/108/113), GET PERFORMANCE descriptor offsets (write speed at +12, kB/s), DVD-R/RW media ID (format 0x0E) **not implemented**, revision byte shown as two hex digits (guess); SPTI with a disc untested; no write commands are ever sent by this module |
| `burner/mmcwrite.rs` | 899 | **Burn Studio M3b, direct write engine ("SPTI Direkt", `io.backend = direct`, opt-in):** `burn_direct_with(&dyn ScsiTransport, Box<dyn SectorSource>, &BurnOptions, &DirectOptions, cancel, tx)` – generic over the transport. Sequence: PREVENT REMOVAL (`lock_tray`) → GET CONFIGURATION (profile → medium kind: DVD+R/+R DL, DVD+RW, DVD-R/-RW/-R DL sequential, DVD-RW restricted overwrite, BD-R, BD-RE; others refused) → READ DISC INFORMATION (write-once media must be blank) → READ FORMAT CAPACITIES (capacity + offered formats) → overburn policy → optional FORMAT UNIT (BD-RE `0x31` without / `0x30` with spare areas, DVD+RW `0x26`, DVD-RW RO `0x13`; waits with TEST UNIT READY) → DVD-R/-RW MODE SENSE/SELECT page 05 (BUFE, test write, DAO/incremental, link size) → SET STREAMING (speed) → OPC → layer break (SEND DISC STRUCTURE `0x20` DVD+R DL, `0x23` DVD-R DL, `0x22` DVD-R DL jump) → RESERVE TRACK → WRITE(10) loop (chunk = `transfer_kib`/2, default 32 sectors; source → ring via `spawn_ring_feed`, or a source with its own ring is read directly; recoverable sense repeated up to `sw_retries` with a pause; READ BUFFER CAPACITY at most every `sample` for `Telemetry.device_percent`, `device_low` waits for the ring) → SYNCHRONIZE CACHE → CLOSE TRACK/SESSION (+R, BD-R: both; DVD+RW: session; DVD-R/-RW incremental: both; DAO/random-writable: none) → optional READ(10) verify against the re-read source → allow removal → eject. Cancel flushes the cache, closes nothing and unlocks. `Overburn` policy: `Overburn` writes past the capacity with a warning, `Prompt` → `NotEnoughSpace` (the UI asks), `Truncate` → refusal with an explanation (a file system image cannot be cut). Test write only on DVD-R/-RW. `DirectOptions::from_settings(&StudioWrite, &StudioIo)`. Windows: `burn_direct(drive_id, spti?, …)` opens `SptiTransport` (+ volume lock) or `ImapiExTransport` (exclusive access held meanwhile) | ✅ 14 unit tests with a scripted fake drive that records every CDB: exact CDB order for DVD+R (streaming/OPC/reserve/4 writes/sync/close track+session/unlock), DVD-R test mode (page 05 bytes: BUFE+test+DAO, link size, no close), DVD-R incremental, BD-RE format with/without spare areas and unavailable format, refusals (test write on DVD+R, non-blank DVD+R, no tray touched), cancel mid-write (2 writes, sync, unlock, no close), retry of recoverable sense (same LBA repeated, lands once; limited; write error not retried), overburn policies, DVD+R DL layer break before the data, device buffer telemetry, verify (OK / mismatch sector), eject after unlock, settings mapping | 🟡 **never run against a real drive**: the MMC field layouts are from the spec/dvd+rw-tools from memory (page 05 bytes, format types, SEND DISC STRUCTURE 0x20/0x22/0x23 parameter layout, CLOSE TRACK number 1, FORMAT UNIT header flags). **M4:** wired — `studio::write` calls `burn_direct` when `io.backend = direct` (transport from `io.transport`), otherwise `OpticalBurner::burn_source`. BD-R/DVD-R DL/DVD-RW sequential erase (BLANK) not implemented – a non-blank sequential disc is refused |
| `burner/power.rs` | 232 | **M3a:** `drive_letter`, `StandbyGuard` (`SetThreadExecutionState`, per thread), `DriveHandle` (`\\.\X:`: `lock_volume`/`unlock_volume`/`dismount_volume`/`prevent_removal`/`eject`/`load` via the documented storage IOCTLs), `TrayLock` (removal lock tied to the open handle); Unix: no-op guard, `Unsupported` errors | ✅ letter parsing, guard create/drop; 🟡 eject/load/lock never run on a real tray (would move the drive) | The lock is released when the handle closes; eject = lock + dismount + allow removal + `IOCTL_STORAGE_EJECT_MEDIA` (each of the first three best effort) |
| `burner/graph.rs` | 538 | **M3a:** `Sampler` (speed from ΔLBA/Δt and the medium's 1x rate, ring %, device %, CPU via `CpuMeter`/`GetSystemTimes`), `speed_summary`, `DateTime` (local time on Windows), `format_template` / `file_name` (`%M %F %D %d %B %T %I %S %V %%`), `folder`, `write_ibg` / `parse_ibg` (`IbgFile`, kinds write/read/verify) | ✅ unit-tested: the template reproduces the sample file name from the plan, `.ibg` text layout byte-exact (CRLF), round trip, tolerant parsing (LF, checksum line, missing columns), sampler maths, summary | 🟡 the trailing ImgBurn checksum line is **not written** (algorithm unknown) – other tools may reject the files; read/verify section names are by analogy; **M4:** `studio.rs` feeds a `Sampler` from the burn telemetry (write) or the read/verify progress and saves one `.ibg` per operation (never with a real disc) |
| `burner/studio.rs` | 875 | **Burn Studio M4, operations layer shared by the GUI and the CLI:** `Ctx {settings, cancel, emit}` / `Event {Progress, Graph (batched ≤ 4/s), Log}` / `Outcome` (the `studio-done` payload); `find_drive`, `transport` (setting `io.transport`: IMAPI-Ex or SPTI; backend `imapi` switches MMC queries off), `drive_details`, `tray`, `open_disc` (raw volume + READ CAPACITY), `open_image` (`.iso`, `.cue` 2048/2352, `.dvd`, `.mds`; a layout file next to an image gives the layer break); `write` (`resolve_speed` = auto-speed rules, `BurnOptions` from the settings, backend dispatch IMAPI `burn_source` vs `mmcwrite::burn_direct`, telemetry → `Recorder` → graph events + one `.ibg` per operation, remaining time via `BurnEta`, copies with a "next disc" wait, user layer break), `write_image`, `write_files` (on-the-fly `stream_image`; with verification the image goes to a temp file first because the source must be read twice), `discovery`, `read`, `verify` (compare or readability only; miscompares named through `isoread::extent_map`), `build`, `on_complete` (`shutdown /s /t 60`, `/h`, standby via `blocking_tool_command`) | ✅ unit-tested: auto-speed rule matching, image opening through sidecars/cue, `write` without a usable drive fails cleanly; the file-only paths run end to end through the CLI (`tests/studio_cli.rs`) | 🟡 **never run with a disc:** `read`/`verify`/`write*`/`discovery` were exercised neither with real nor fake media at this layer (the pieces below were: `FakeDisc`, fake transport). Settings the engine ignores (stored, shown disabled in the UI): `general.md5_compare`, `general.warnings.*`, `read.hw_retries`/`priority`/`batch_eject_next`, `build.dvd.uppercase`/`ifo_bup_ecc_padding`/`read_dvd_text`, `write.mode`/`dvdpr_min_radius`/`dvdpr_dl_only_l0`/`burn_proof`/`fastwrite`/`bdr_verify_not_required`/`cycle_tray_before_verify`/`auto_book_type`/`eject_after` (use `io.eject_after.write`), `verify.hw_retries`/`ignore_non_user_data`, `io.buffered_io_*`; `write.test_mode`, `overburn`, `write_type`, `link_size`, `opc`, … act only with backend `direct`; on-complete "standby" (`SetSuspendState`) untested; Unix: device access reports "nicht unterstützt" |
| `studio_cli.rs` | 278 | **Burn Studio CLI** (module of the binary): `read-disc OUTPUT [--drive]`, `verify-disc [IMAGE] [--drive]` (without an image: readability check), `build-iso SOURCES… --output X.iso [--label] [--as-disc\|--as-data]` (a DVD/BD folder under the default `prompts = ask` is refused with the flags named), `iso-label ISO [--set NEU]`, `ifo-layerbreak ISO\|FOLDER [--all]` (candidate table), `drive-info [--drive]`; settings from `%APPDATA%\convertx-rust\settings.json` (`burn_studio` section), progress on stderr, Ctrl+C cancels (exit 130) | ✅ `build-iso`, `iso-label`, `ifo-layerbreak` (error path), argument errors, missing inputs (`tests/studio_cli.rs`) | 🟡 `read-disc`, `verify-disc`, `drive-info` with a disc never run (the tests run the binary with an empty `APPDATA`, i.e. default settings) |
| `burner/testutil.rs` | 109 | `#[cfg(test)]` synthetic VIDEO_TS folders (pseudo-random files; `synthetic_dvd` with real VMG/VTS IFO structures: PGCIT, cell playback, C_ADT) | ✅ | — |
| `burner/udf.rs` | 1572 | **One UDF 1.02 writer for everything:** `plan_tree(label, &[TreeEntry], &TreeOptions)` lays out any tree (UDF 1.02 + ISO 9660 level 1 names with `~N` collisions, optional **Joliet** (UCS-2, ≤ 64 chars, extension kept), depth ≤ 8 in the ISO views (deeper dirs: warning, UDF keeps them), files > 4 GiB as multi-extent records, ISO area behind the anchor when it does not fit before sector 256, `start_lba`, `min_sectors`, dedupe by size + MD5); `plan_image`/`plan_image_with` are thin wrappers for DVD-Video (`dvd_file_order`, padding, overrides, optional `AUDIO_TS`); `write_to(plan, impl Write, …)` writes strictly front to back into any sink (`write_image` = file wrapper); `TreeEntry` (+ `scan`, `Transform` hook for streaming changes while copying), low-level helpers shared with `udf250` | ✅ **golden hash** (synthetic VIDEO_TS image, fixed timestamp) unchanged by the refactor; ✅ unit tests (deep tree + Joliet + collisions + dedupe, long Joliet names, min size/start sector, 5 GiB multi-extent records, ISO area behind the anchor with 9000 files, rejected trees, level-1 names); ✅ mounted by Windows (DVD image, and a data tree with Joliet/UDF, long non-ASCII names, a duplicate: every file byte-identical, `tests/disc_image.rs`) | ISO views are read back only by our own `isoread` and Windows (UDF side); `udfinfo`/`isoinfo` not run on tree images; Joliet seen only through `isoread::open_joliet`; the plan's `iso_level1_names` option was dropped (always level 1) |
| `burner/udf250.rs` | 831 | **UDF 2.50 writer for Blu-ray** (§2.8): `plan_entries` / `plan_bd_image` (generic tree, addresses aligned to 32), `write_bd_image` (file wrapper) / **`write_bd_to`** (any sink), `create_bd_image`, `is_dual_layer`, `BD25_SECTORS` / `BD50_SECTORS`; `TreeEntry`/`scan` now live in `udf.rs` (re-exported), `Transform` (CPI reset) applied while copying; `TreeEntry.data`/`padding_before` are ignored here | ✅ unit-tested (tags v3, FIDs, EFE layout, partition maps, 3 GiB file extents, mirror = copy, cancel); ✅ **mounted by Windows as UDF with every file byte-identical** and ✅ read by ffmpeg's libbluray (`tests/bluray.rs`) | `udfinfo` (WSL) not run; no ISO 9660 side; never burnt |
| `burner/layerbreak.rs` | 309 | Cell-start parsing, layer-break choice (`fits` is `pub(crate)`), padding + `TT_SRPT` patch, `plan_disc` and **`plan_disc_with`** (optional `AUDIO_TS`, user-chosen `LayerBreak` that is checked against the capacities) | ✅ logic + consistency tests | Seamless flags of cells are only considered by the Burn Studio inspector (`tools::ifo_candidates`), not by the automatic choice; real DL media never burnt |
| `burner/windows.rs` | 853 | (M3b: `volume_paths` is now `pub(super)` for `mmcwrite::burn_direct`.) IMAPI2 burner (see §2.6), incl. write-event sink `WriteEvents` + `EventConnection` (advise/unadvise around `Write`), BD-R / BD-RE media (SL/DL by capacity, BD-RE overwritten like DVD+RW); **M3a:** `burn_iso` and the new **`burn_source`** share `burn_feed` (the old path is unchanged: file feed, seekable stream); `burn_source` pumps any `SectorSource` through a ring buffer (`source::spawn_ring_feed`, size/recovery from `BurnOptions`; a source with its own ring, i.e. `StreamSource`, is read directly), verifies by re-reading the source when it is rewindable, honours `prevent_standby` (`StandbyGuard`) and `lock_tray` (`TrayLock`, dropped before the eject), and feeds `BurnOptions.telemetry` from IMAPI's write events (real LBA) or the stream position (device buffer is always `None` under IMAPI) | ✅ enumeration/probing verified on real hardware; ✅ write, finalize, eject, read-back verification and burn ETA **reported working by the user** on the Pioneer BDR-209M (2026-10-15, before the event sink); ✅ event mapping unit-tested and the sink advised on a real `MsftDiscFormat2Data`; 🟡 events during a real write, erase of RW media and the DL layer break never run; 🟡 BD-R/BD-RE probing and writing never run (no blank BD tried) | Whether IMAPI calls the vtable `Update` or `Invoke` is only known after a real burn (both are handled) |
| `burner/unix.rs` | 259 | growisofs/xorriso burner; processes via `tools::blocking_tool_command` | 🟡 compiles for `x86_64-unknown-linux-gnu`; never run | Its unit tests only compile-checked |

### 3.2 Desktop app (`src-tauri/`, Tauri 2.12)

| File | Lines | Responsibility | Status |
|---|---:|---|---|
| `src/main.rs` | 825 | Window handler: closing the main window destroys the `menu-editor` and `burn-studio` windows; closing `burn-studio` while the main window is hidden (start with `--burn-studio`) exits the app; `setup` creates the `burn-studio` window (`burnStudio.html`) and hides `main` for `--burn-studio`. Commands `probe_file`, `list_templates`, `template_preview`, `calculate_project` (uses the profile from the settings and the disc's limits; BD menus from `bdMenuSecs` × 1.1 MB/s), `start_conversion` (with optional `customLayout`), `cancel_conversion`, `default_output_dir`, `startup_files`, `get_settings`, `save_settings`, `default_settings`, `settings_info`, `title_frame` (video frame as data URL for the editor); events `convert-event` (engine `Event`, incl. `eta`), `convert-done` (with `videoTs`, `projectName`), `convert-cancelled`. Conversion runs on its own thread + Tokio runtime. | ✅ |
| `src/burn.rs` | 461 | Commands `list_drives`, `disc_needs`, `probe_media(driveId, videoTs, speedPref, autoErase)`, `start_burn`, `cancel_burn`, `startup_burn` (GUI started with a DVD folder argument opens the burn dialog), `open_folder`; events `burn-progress` (`BurnProgress` + `eta`), `burn-done`. Media view with problem/warning/needs-erase/auto-start rules and per-speed duration estimates; ISO at `<output>/<project>.iso` (numbered if taken); safe post-burn deletion (only folders containing nothing but `VIDEO_TS`/`AUDIO_TS` or `BDMV`/`CERTIFICATE`); `DiscNeeds` via `DiscContent::of_folder` (+ `bluray`), BD speed labels/estimates, `startup_burn` accepts a `BDMV` folder. | ✅ logic unit-tested; ✅ real burn reported working by the user |
| `src/burn_studio.rs` | 497 | **Burn Studio window backend (M4):** one operation at a time (`StudioState`: busy flag, cancel flag, tray-lock holder thread); `run()` starts `studio::*` on a thread with the events `studio-progress` (phase, fraction, lba, speed x, ring %, device %, eta), `studio-graph` (sample batches ≤ 4/s), `studio-log`, one `studio-done` (`Outcome`) and runs the on-complete action. Commands `studio_drives` (sorted by letter/name), `studio_drive_info` (IMAPI media + MMC details, each may fail alone), `studio_eject`/`studio_load`/`studio_lock`, `studio_write_image` (+ delete-after), `studio_write_files`, `studio_discovery`, `studio_read`, `studio_build_plan` (→ `PlanInfo`, asks via `needsAnswer`), `studio_build`, `studio_inspect` (sizes + detected content of the file list), `studio_verify`, `studio_cancel`, `studio_label_read`/`studio_label_patch`, `studio_ifo_candidates`, `studio_sidecar_write` (`.dvd`/`.cue`/`.mds`), `studio_graph_load`/`studio_graph_folder`, `studio_template_preview`; settings via the shared `get_settings`/`save_settings` | ✅ compiles (`cargo check`); 🟡 no Rust unit tests, no command was invoked from a running Tauri window |
| `src/menu_editor.rs` | 179 | Commands `check_layout` (validation against the project's titles; frame name `PAL`/`NTSC`/`BD1080`/`BD720` selects DVD or Blu-ray rules), `text_lines` (engine line breaks of a text element for the editor, same frame names), `load_layout`, `save_layout`, `image_preview` (images only, ≤ 25 MB) | ✅ unit-tested |
| `capabilities/default.json` | 19 | Windows `main`, `menu-editor`, `burn-studio`: core, create/focus/close windows, dialog open/save | 🟡 never loaded by a real Tauri window (the studio page was only run in a browser with a mocked `invoke`, §5.2) |
| `tauri.conf.json` | — | Window, CSP, NSIS bundle with `bin/*.exe` as resources | ✅ installer reported working by the user (2026-10-15; not built or checked in this session); release exe built with `--features tauri/custom-protocol` |

### 3.3 Frontend (`frontend/`, vanilla TS + Vite 7)

| File | Lines | Responsibility | Status |
|---|---:|---|---|
| `index.html` | — | Shell, "💿 Brennstudio …" button (creates or focuses the `burn-studio` window like the editor), disc picker with BD-25 / BD-50, conversion overlay (5 phase chips, total/step bars, ETA line, log, cancel, "Auf Disc brennen …") | ✅ |
| `src/main.ts` | 1314 | Title list (drag & drop), per-title audio/subtitle/chapter/label editing, menu tab with **"ConvertX-Vorlage" / "Eigenes Menü (Editor)"** switch (template picker with preview, or custom layout summary + live validation), `editorTitles()` (index, label, path, chapter count and source chapter names of the live project), layout↔videos binding (`customLayoutKey` in localStorage), disc picker incl. **BD-25 / BD-50** (quality tooltip, "BD-50 wählen" hint, on a BD the menu tab offers "Ohne Menü (Autoplay)" / "Eigenes Menü (Editor)"; **one menu slot per medium** (`slots`, `switchMedium`; localStorage `convertx-menu-layout` for DVD, `…-bd` for BD), `frameName()` = `PAL`/`NTSC`/`BD1080`/`BD720`, an invalid BD layout blocks converting, template motion toggle off, single-video check), disc fill gauge, convert/cancel (blocked while the layout is invalid or made for other videos), ETA display, opens burn dialog after success | ✅ screenshot-verified |
| `src/menuEditor.ts` | 1610 | WYSIWYG menu editor, mounted by `mountMenuEditor` into the editor window (see §2.5); **multi-select** (Shift+click, marquee, Alt+click = single element), **groups** (Ctrl+G / Ctrl+Shift+G, `group` field), clipboard (Ctrl+C/X/V/D), group move/resize/arrow keys/delete as one undo step, **layer list** (select, locate pulse, hover outline, collapsible groups); "Übernehmen" keeps the editor open; `requestClose()` for the window close button: menu manager, 4:3 stage (16:9 with square pixels on a Blu-ray, `isBdFrame`, 255-button counter) with drag/resize, properties, actions, validation, draft, load/save; "Zeilenumbruch" checkbox (new texts wrap; lines from the engine via `text_lines`), "Eigene Farben" per button, pictures/videos/backgrounds previewed with the TV's pixel shape (`fitOnScreen`); opens a fresh draft when the stored layout was made for other videos (old one reachable via undo); video badges and chapter pickers show the real title labels / chapter names | ✅ screenshot-verified before the 2026-10-17 rework (open, select, drag, add text, apply; 4-title project); 🟡 the window move, multi-select, groups, clipboard and layer list (2026-10-17: `tsc` + build only, never clicked); 🟡 the 2026-10-15 additions (wrap checkbox, own colours, pixel-shape previews) only compiled (`tsc`), not clicked |
| `src/layout.ts` | 410 | `isBdFrame`, `canvasSize` (BD: 1920×1080), `pixelAspectOf`, `menuClipSecs`, `maxButtons`, `toBluray` (a DVD-geometry draft scaled to the 16:9 canvas, 400×225 video boxes); layout types (mirror of `layout.rs`, incl. `wrap`, `highlight`), fonts list, `draftLayout` generator for the project's real titles (`EditorTitle.index`, labels, chapter counts, source chapter names), `titlesKey`, `madeForOtherTitles`, `chapterLabel`, helpers | ✅ 4-title draft passes `check_layout` and a full build with dvdauthor |
| `src/settings.ts` | 633 | "Allgemeine Einstellungen" modal (tree: Allgemein, Ausgabe › TV Format/Enkodierung/Videobearbeitung, Menü Optionen, Kapitel, Untertitel, Audio, **Brennen**); Half D1/SIF and SP/MP/LP selectable with explanatory texts (bitrate ceilings, LP → Half D1, 4:3 only); "Schnelles Encoding (Entwurf)" checkbox under Enkodierung › Leistung; group **Blu-ray** under TV Format (`bd_resolution` 1080p/720p, frame-rate note, `bd_single_video`, **`bd_hw_encode`** checkbox), BD ceilings in the profile hint, speed hint per medium; parallel-encodes hint names the default (half the cores) | ✅ compiles (`tsc`); 🟡 the new checkboxes (fast encoding, NVENC) not clicked in the GUI |
| `src/burn.ts` | 562 | Burn dialog: drive list, media indicator, label (live sanitised), speed list with "(empfohlen)" + duration estimate, checkboxes (verify, keep ISO, eject, delete after, erase confirmation), 20 s countdown, phases Image/Formatieren/Schreiben/Abschließen/Überprüfen, live ETA "[4/5]/[5/5]", two-click cancel, opens Explorer when closed without burning (unless quiet); media labels BD-25/BD-50, "Blu-ray"/"DVD" wording, needs text "BD-50 (Dual Layer) nötig" / "passt auf eine BD-25" | ✅ UI verified without disc; ✅ burning reported working by the user |
| `src/editorWindow.ts` | 26 | Entry of `editor.html`: listens for `editor-init` (data from the main window), mounts the editor, relays applied layouts with `emitTo("main", "editor-apply")`, destroys the window on close, `onCloseRequested` → editor's discard confirm; then emits `editor-ready` | 🟡 compiled only |
| `editor.html` | — | Second Vite page of the editor window (loads `styles.css` + `editorWindow.ts`); `vite.config.ts` lists it as a build input (`dist/editor.html` checked) | 🟡 |
| `burnStudio.html` | 13 | Third Vite page: the Burn Studio window (`styles.css` + `src/burnStudio.ts`); `vite.config.ts` input `studio` (`dist/burnStudio.html` checked) | 🟡 |
| `src/burnStudio.ts` | 383 | Entry of the Burn Studio window: menu bar (Datei/Ansicht/Modus/Werkzeuge/Hilfe), home "Was möchten Sie tun?", drive bar (drive, media text with media ID, speed list from the medium, eject/load/info), status panel (phase, progress, main/device buffer bars, speed, sector, remaining time, log, cancel), graph tab, event wiring (`studio-progress/graph/log/done`), close request cancels a running operation, "wenn fertig: Fenster schließen" | ✅ (browser, mocked `invoke`) menu, mode switch, drive bar, a simulated write with progress + graph + done; 🟡 never in a real Tauri window |
| `src/studio/api.ts` | 245 | Typed command/event wrappers and DTOs (mirror of `burn_studio.rs`, `BurnStudioSettings`) | ✅ `tsc` |
| `src/studio/ui.ts` | 89 | `el`, formatters, `modal`, `field` | ✅ |
| `src/studio/views.ts` | 393 | The 6 modes: write image, write files, read, build (shared file list with sizes, content badge, fill gauge vs. target medium, label, plan info, "Als DVD-Video aufbauen?" prompt, layer-break chooser), verify (compare / readability), Discovery (with confirmation) | ✅ (mock) write image, build view rendering, fill gauge; 🟡 file/folder dialogs, drag & drop (**not implemented**: buttons only), the other views never clicked |
| `src/studio/graph.ts` | 165 | Canvas graph: speed (left axis) + main/device buffer + CPU (right axis, %), hover tooltip, live pushes and loaded `.ibg` | ✅ (mock) live samples drawn; 🟡 a loaded `.ibg` not clicked |
| `src/studio/settings.ts` | 346 | Settings dialog with 7 tabs (Allgemein, Lesen, Image bauen, Schreiben, Überprüfen, Graph, E/A) bound to `burn_studio`; ignored options disabled with a reason, direct-engine-only options disabled unless backend `direct`; file-name template preview | ✅ (mock) opened, tabs switched, disabled hints; 🟡 saving through the real `save_settings` not clicked |
| `src/studio/tools.ts` | 196 | Tool modals: volume label, IFO/layer break entry, layout files (`.dvd`/`.cue`/`.mds`), drive/disc info, auto-speed table | 🟡 compiled only |
| `src/studio/layerbreak.ts` | 86 | Layer-break modal: candidate table (VTS/TTN/PGC/cell, L0/L1, split %, padding, flags), "interleaved" filter, pick → `BuildInput.layerBreak` | 🟡 compiled only |
| `src/label.ts` | 23 | Volume-label rules (same as Rust; cross-checked with identical cases) | ✅ |
| `src/api.ts` | 353 | Typed `invoke`/`listen` wrappers and DTOs (+ editor commands) | ✅ |
| `src/styles.css` | 2058 | Light/dark theme tokens, layout, menu editor, Burn Studio window | ✅ |

### 3.4 Native tool build (`tools/dvdauthor-win/`)

`build.sh` builds static `dvdauthor.exe`/`spumux.exe` from **ldo/dvdauthor commit `fe8fe35`**
in MSYS2 (MinGW-w64) with shims `wincompat.h`, `include/langinfo.h`, `include/netinet/in.h`;
`README.md` documents it; GPL licence text is shipped as `bin/dvdauthor-COPYING.txt`.

---

## 4. External dependencies & tools

### 4.1 Runtime tools

| Tool | Version in use | Required | Notes |
|---|---|---|---|
| `ffmpeg` / `ffprobe` | 9.0.1 full build (gyan.dev), in `bin/` | yes | **NVENC (optional, Blu-ray) needs NVIDIA driver ≥ 610**; the dev machine had 591.86 (`h264_nvenc` failed: "required nvenc API 13.1, found 13.0", build fell back to x264) and now has 617.42, where it works. Must be the real binaries (scoop shims without `.shim` are rejected by the `-version` check). Needs `mpeg2video`, `ac3`, `dvd` muxer, `lavfi`, `bwdif`; **`libx264`** for Blu-ray output (and tests). The `bluray` protocol (libbluray) is only used by a test. |
| `dvdauthor` / `spumux` | self-built from ldo/dvdauthor `fe8fe35`, static, in `bin/` | yes | Rebuild with `tools/dvdauthor-win/build.sh`. A WSL dvdauthor exists on the dev machine but is unused. |
| `tsMuxeR` | 2.7.0 (justdan96/tsMuxer, Apache-2.0), `bin/tsMuxeR.exe` tracked in git, licence in `bin/tsMuxeR-LICENSE.txt` | for Blu-ray only | Optional: DVD builds work without it; a BD build without it fails naming `bin/tsMuxeR.exe`. Bundled by the installer glob `../bin/*.exe`; the licence is listed in `tauri.conf.json`. Writes `BDMV` only (no `CERTIFICATE`). |
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
| `md-5` 0.10 | engine | MD5 of images/reads (Burn Studio; golden-hash test, dedupe in `udf.rs`, MD5 in `builder`/`reader`/`verifier`) |
| `regex` 1 | engine | Parsing helpers |
| `windows` 0.62 (+ `windows-core`) | engine, Windows only | IMAPI2 COM: features `Win32_Foundation`, `Win32_Storage_Imapi`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`, `Win32_System_Ole`, `Win32_System_Variant`; **M3a:** `Win32_Security`, `Win32_Storage_FileSystem`, `Win32_Storage_IscsiDisc`, `Win32_System_IO`, `Win32_System_Ioctl`, `Win32_System_Power`, `Win32_System_SystemInformation`, `Win32_System_Threading` (SPTI, storage IOCTLs, standby, `GetSystemTimes`, `GetLocalTime`) |
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

### 5.1 Test suite (2026-10-24: **338 tests** – last full `cargo test --workspace` run, after Burn Studio M4: all 338 green; includes 1 test (`render_menu`: icon buttons with outside captions) added by a parallel session in the same checkout)

`cargo test --workspace` → engine unit tests 262, GUI unit tests 13, integration tests 63 (CLI binary 0, doc tests 0):

| Test file | Tests | What it proves (real tools unless noted) |
|---|---|---|
| unit (`src/**`) | 262 | **Burn Studio M4 (3 new):** auto-speed rules (media ID first, then drive, explicit speed wins), images opened through `.dvd`/`.cue`/`.mds` sidecars, `write` without a usable drive fails cleanly; **Burn Studio M3b (14 new):** direct write engine against a scripted fake drive (see its row in §3.1); **Burn Studio M3a (29 new):** CDBs byte by byte, sense decoding, response parsers, `drive_info` with a scripted fake drive, read-only host probe of the real drive, drive-letter parsing, standby guard, file-name template (the sample name from the plan), `.ibg` write/parse round trip, sampler/summary maths, `SourceReader`, `spawn_ring_feed` (delivery, source hand-back, abort on a failing source), `rewindable`/`ring_stats`; **Burn Studio M2:** UDF tree writer (deep tree, Joliet, collisions, dedupe, long names, min size/start sector, 5 GiB multi-extent, ISO area behind the anchor), ring buffer (recovery state machine, wrap-around, threaded transfer, cancel), sources (file/zero/2352/sequential/stream/discovery), reader (retries, bad sectors, cancel, split, layouts), verifier, builder (see its row in §3.1); **Burn Studio M1:** settings defaults/old files/unknown choices and clamps, `.dvd`/`.cue`/`.mds` formats, ISO 9660 reader (listing, case-insensitive find, extent map, multi-extent, non-ISO), label patching (byte-identical to fresh DVD and BD images, Joliet), layer-break candidates; **Blu-ray menus:** IGS RLE round trip / 14-bit runs / segment and field lengths / fragmentation / colour conversion, HDMV command bytes (compared with tsMuxeR's files) and `index.bdmv`/`MovieObject.bdmv` layouts, PGS→IG patch (CRC-32 check value, byte-exact diff of a real tsMuxeR playlist, CLPI stream entry, TS packets), `bd_menu` (object numbering, every button action, still vs motion menu objects, auto-start, title-end behaviour for default / loop / sequential / sequential+loop / own menu, rendering a page into IG objects), per-title and menu meta files (`--mplsOffset`); **Blu-ray:** disc limits/targets/overhead/quality/profile ceilings, every branch of `bd_frame_rate`, harmonising of tracks, BD filter chain (pillarbox, bwdif, 720p), NVENC args (options, no x264 params, `p3` fast, `-multipass fullres` only for 2-pass and no analysis run), NVENC failure-reason mapping, `bd_hw_encode` default/backward compatibility, x264 flags (`fake-interlaced` only for 1080 TV rates, GOP, fast preset), audio padding/filler tracks, 2-pass, SRT/PGS extraction by track, `bd.meta` text, chapter marks, language codes, `empty_pgs`, UDF 2.50 (tags v3, FIDs, EFE layout, metadata partition map, 3 GiB extent split, alignment, written image structure incl. identical mirror, cancel), media/content cross cases, `bd_media_type`, BD speeds/ETA, `bd_resolution` settings; Word wrap (spaces, long words, `\n`, height cut with "…"), button colour pairs (limit 3, shared colours rejected, one pixel value per pair in both masks), pictures fitted with the TV pixel shape (contain, rotated, stretch), Bitmap subtitles (PGS RLE + display sets, SPU RLE + control sequences, VobSub pictures with/without palette, `.idx` header, hex dump/JSON, canvas mapping, quantising, spumux XML), Frame sizes and Half D1/SIF filter chains (letterbox in the reduced frame), profile ceilings (`calculate_capped`), LP → Half D1 rule, settings → `Resolution`/`EncodingProfile`, IMAPI write-event mapping (actions → phases, LBA fraction relative to the start, clamping, no report without sector count) and advising the event sink on a real IMAPI format object, fast-encoding args (no `-trellis` in 1-pass/analysis/final runs, everything else identical), parallelism default = `default_jobs()`, custom layouts (`group` field round trip / optional, settings menus and stream selection, background shading graph, JSON round trip, DVD-limit validation, cross-domain navigation commands, dispatcher command limit, layer/mask/spec rendering, NTSC scaling), custom menu filter graph, parsers, bitrate model, filters/args, XML generation, label rules, CRC/tags/dstrings/extents of the UDF writer, layer-break choice + `TT_SRPT` patch, speed mapping, `BurnEta`, `FrameEta`, config sanitising, ffmpeg progress parsing, verify mismatch detection, IMAPI drive enumeration (real drive, read-only) |
| GUI unit (`src-tauri`) | 13 | Live estimate follows the profile (LP caps at 4000 kbps), Blu-ray estimate (BD limits, no menus without a layout, 1.1 MB per menu second with one, BD ceilings, quality), Blu-ray frame names in `check_layout` (16:9 rule, chapter check), BD media/speed views (BD-25 content on BD-50 fine, BD-50 on BD-25 rejected, BD labels), `BDMV` deletion rules, Layout save/load round trip and error path, layout check against project titles, media-view rules (auto-start only on suitable media, RW erase rules, DVD-9 needs DL, speed estimates), ISO path numbering, safe post-burn deletion, project calculation |
| `build_dvd.rs` | 1 | Two clips (16:9 5.1 + stereo, German SRT with umlauts, chapters) → full DVD-5 with sub-menus; stream codecs in VOBs |
| `two_pass.rs` | 5 | 1-pass/2-pass args, real encodes produce a DVD program stream (MPEG-2 720×576 + AC-3), no `.m2v`/`.ac3`/pass logs left |
| `motion_menu.rs` | 7 | Motion layers/slots on the real template, overlay pipeline produces DVD MPEG, full build with looping menus, sound start per chapter page |
| `ntsc.rs` | 2 | NTSC 720×480 @ 29.97 for titles, still and motion menus, masks, subtitles; menu sound per video |
| `playback_options.rs` | 3 | Auto start / sequential / loop; single title without title/root menu |
| `custom_menu.rs` | 5 | **Button colours and word wrap on the disc:** 3 colour pairs → spumux "PNG had 4 colors", the PCI button colour table of the VMGM menu maps pixel values 1/2/3 to the 3 pairs (VMGM PGC palette read from the IFO, ±3 per channel), each button's mask in its own colour, the wrapped heading drawn in 3 line bands; Settings menu with `setAudio`/`setSubtitle` (2 audio tracks, SRT subtitle), chapter video buttons from each chapter start, vignette/bottomDark/dim shading → real build accepted by dvdauthor; custom layout → full DVD with real ffmpeg/spumux/dvdauthor: blurred image background, video button, rotated image, sound, video background; dispatcher (`if (g2 eq 102) jump menu 3`), chapter dispatcher, VMGM router, `g2 = 0` resets accepted by dvdauthor; menu VOB has MPEG-2 + AC-3 + subpicture, ~3 s loop; mask uses only layout colours; NTSC canvas → 720×480; invalid chapter fails before encoding |
| `burn_flow.rs` | 13 | `burn_video_ts` with a fake `OpticalBurner` on a real small DVD: complete image reaches the burner, options/speed passed through, image removed (or kept with `keep_iso`), phase order and remaining time (0 at Done), layer break handed over on a forced dual-layer plan, backend error and cancel (during image and during write) leave no `.iso`/`.iso.part`, a cancelled image phase keeps an existing file at the ISO path; **Blu-ray folder (synthetic, no tools needed):** the UDF 2.50 image reaches the burner complete, `layer_break == None`, image removed/kept (`BEA01`/`NSR03`, no ISO 9660), failure/cancel leave no `.iso`/`.iso.part`, an existing file survives a cancelled image, `DiscContent` recognition; CLI `burn`: `--speed 99` → exit 2, missing VIDEO_TS → exit 1, unknown drive → exit 1 without writing (lists the real drives read-only; skip with `CONVERTX_SKIP_DRIVE_TESTS`) |
| `studio_cli.rs` | 4 | **Burn Studio CLI** (binary run with an empty `APPDATA`, no drive, no tools): missing/invalid arguments → exit 2 (also `--as-disc` + `--as-data`); missing sources/images → exit 1; `build-iso` of a data folder (label sanitised, ≥ 600 sectors) → `iso-label` read → `--set` → read again; `ifo-layerbreak` on a data image fails; a DVD-Video folder is refused under `prompts = ask` (names `--as-disc`/`--as-data`) and builds with either flag |
| `process_hygiene.rs` | 1 | No `Command::new(` in `src/` or `src-tauri/src/` outside `src/tools.rs` (guard for the no-console-window rule) |
| `cancel.rs` | 1 | Cancel during encoding kills tools and removes the work dir |
| `disc_image.rs` | 4 | **Data tree mounted by Windows** (`data_tree_mounts_with_long_names`: builder image with Joliet + UDF, deep folders, long non-ASCII names, a duplicate, an empty file: UDF label, every file byte-identical); **Golden hash** (`golden_video_ts_image_hash`, no tools needed): a synthetic VIDEO_TS image with a fixed timestamp has MD5 `52152f47535751cef7fc68aae5889f00` (captured before the Burn Studio refactor of `udf.rs`; any change of the VIDEO_TS layout must be deliberate); ISO of a real DVD **mounted by Windows** (`Mount-DiskImage`): FS = UDF, label correct, every file byte-identical; layer-break plan on injected small capacities lands on a cell start, %16, L0 ≥ L1; forced padding + patched `TT_SRPT` keeps title→VTS addresses consistent and mounts correctly |
| `real_templates.rs` | 2 | All installed "Open Style" templates parse |
| `render_menu.rs` | 3 | Single and paginated menu rendering on the real template |
| `bitmap_subs.rs` | 2 | PGS: a `.sup` written by the test (white box with black border, forced second picture) muxed into MKV by ffmpeg → spumux XML times 1.00–2.50 / 3.00–3.60 + `force`, "2 subtitles added", 2 subpicture packets in the VOB at +1 s/+3 s, overlay pixel at the mapped box centre white like ffmpeg's rendering of the source; VobSub: DVD built from an SRT used as the input of a second build (VOB without palette) → all 3 subtitles with the same timing |
| `resolution.rs` | 3 | Real builds: Half D1 PAL from a 16:9 source → title VOB 352×576 4:3 + warning, dvdauthor `aspect="4:3"`, menus 720×576; SIF NTSC + MP → 352×240, 6000 kbps ceiling; LP on a 13 MB custom target → bitrate < 3000 kbps → Half D1 352×576 |
| `bluray.rs` | 7 | **(new, `mod menus`)** Two clips + a 3-menu layout (title / root / settings) at 720p: playlists/clips 00000–00004 (and BACKUP copies), each menu clip H.264 1280 wide with a **0x1400 stream** whose PES start with ICS (0x18) … PDS (0x14) / ODS (0x15) … END (0x80) and no 0x1200 left, CLPI says 0x91/0x1400 and MPLS has 0 PG / 1 IG with still mode, titles keep their PGS track and chapter marks, `index.bdmv` (First Play 0, Top Menu 1, titles → objects 5, 6), `MovieObject.bdmv` with 7 objects (= BACKUP copy), an invalid (4:3) layout fails with the 16:9 message; **earlier tests:** | Two clips (16:9 25 fps with 5.1 + stereo AC-3, SRT, chapters 0/6 s; 4:3 with one stereo track) → `BD25`/PAL/1080p → `BDMV/index.bdmv`, `MovieObject.bdmv`, `PLAYLIST/00000.mpls`, `CLIPINF`, one `STREAM/00000.m2ts`, `CERTIFICATE`; ffprobe: h264 High level 41 1920×1080 25 fps, 2 AC-3 (6 + 2 channels), 1 PGS, 18 s; the second audio track is audible during A and **silent (< −80 dB) during B**; the playlist has **3 marks** (= 2 + 1 chapters, tsMuxeR adds none), 6 s / 12 s apart; 720p NTSC single clip → 1280×720 at 60000/1001; `--single-video` with two inputs, FullD1 on a BD, HD on a DVD fail before anything is created; the build's **UDF 2.50 image**: LVD domain revision 0x0250, two partition maps (type 1, type 2 metadata), **mounted by Windows as `UDF` with every BDMV/CERTIFICATE file byte-identical**, and (separate 200 s project) **read by ffmpeg's `bluray:` protocol (libbluray/libudfread)** both as folder and as image |

Integration tests **skip with a message** (not fail) when ffmpeg/dvdauthor/spumux or the
ConvertX templates are missing.

### 5.2 Verified outside the test suite (manual runs, 2026-10-14/15/17/24)

- ✅ **Burn Studio M4, window in a browser with a mocked Tauri (2026-10-24):** `vite` dev server + the built-in browser pane; a stand-in for `window.__TAURI_INTERNALS__` answered `invoke` with canned data (drive `PIONEER BD-RW BDR-209M (F:)`, a blank DVD+R with speeds 5540/11080/16620, a DVD-Video plan, one layer-break candidate) and fired `studio-progress`/`studio-graph`/`studio-done`. Seen working: home cards, menu bar and mode menu, drive bar with media text/media ID and the speed list (8x/16x/24x labels), "Image auf Disc schreiben" → progress 55 % / 6.0x / ring 79 % / "Laufwerkspuffer —" / remaining 1:30 → "Fertig", the graph tab drawing the four curves (screenshot), the build view with the fill gauge against a DVD-5, the settings dialog (tab "Schreiben": options only the direct engine honours greyed out with the reason). **Not a real window:** no Tauri command, capability, file dialog, drag & drop, window creation or `--burn-studio` start was exercised.
- ✅ **Burn Studio M3a host probe (2026-10-24, `spti::tests::host_probe_reads_only`, Pioneer BDR-209M on `F:`, empty drive):** through `ImapiExTransport` INQUIRY answers `PIONEER` / `BD-RW   BDR-209M` / `1.33`, GET CONFIGURATION lists 21 profiles and reports the current profile 0 ("keine Disc"), TEST UNIT READY not ready; **`SptiTransport` opened `\\.\F:` read/write from a non-elevated process** and returned the same model (so on this machine "SPTI Direkt" needs no administrator). Only read commands were sent. Everything that needs a disc (capacity, disc information, physical format, media ID, write speeds) was not exercised.

- ✅ **Blu-ray menus in VLC 3.0.23 (libbluray, 2026-10-17):** (1) *spike:* hand-built 2-button ICS +
  1 s clip + title through tsMuxeR (`--mplsOffset/--m2tsOffset`), patched: libbluray printed
  "First play: 1, Top menu: 1, HDMV Titles: 1", "Still image (infinite)", ES 0x91 on PID 5120; the menu
  showed, Down moved the highlight, Enter played the title playlist. tsMuxeR copies the segments
  byte-identically as PCS-labelled PGS and rejects/drops a real ICS type. (2) *full CLI build*
  (`build-dvd --disc bd25 --layout … --resolution hd720`, two 25 fps clips of 3 s and 20 s with
  2 audio tracks + SRT + chapters every 6 s, a still menu, a motion menu (blurred video background,
  video element, title audio, 6 s) and a settings menu): First Play shows the still main menu with the first
  button highlighted (glyph-shaped yellow highlight); the motion menu plays background + video and
  **keeps the selected button across its 6 s loop**; "Film 1" plays and playback **returns to the main
  menu** at the title's end; "Film 2 ab Kapitel 2" starts at 8.2 s (chapter mark 1 + 2 s); **Play all** runs
  title 1 then title 2 (2.8 s into it after 6 s) and returns to the menu afterwards; the settings
  menu opens; **"Untertitel an"** then playing title 2 shows the subtitle "Untertitel EINS".
  Not checked audibly: the audio-switch buttons (`SetStream` audio), "Untertitel aus", "Zurück" from the
  settings menu, auto-start, sequential/loop playback options (only unit-tested).
- ✅ **GUI, Blu-ray menu path (release exe, 2026-10-17):** BD-25 selected, one video (`t2.mkv`) added through
  the file dialog → tab Menüvorlage shows "Ohne Menü (Autoplay)" / "Eigenes Menü (Editor)"; with the editor
  mode and no layout "Konvertieren" is disabled; "Menü gestalten …" opens the editor on a **16:9 stage** with
  the BD draft (3 menus: Hauptmenü, t2 – Menü, t2 – Einstellungen), counter "2/255 Buttons", "✓ Layout ist
  gültig (3 Menüs)"; "Übernehmen" shows the summary card ("Passt zu den aktuellen Videos"), "Konvertieren"
  enabled and the size gauge rises. Not done there: dragging/editing on the BD stage, switching DVD ↔ BD
  and back (slot logic is unit-untested frontend code, only compiled), a conversion started from the GUI.
- ✅ ffprobe on a patched menu clip lists the IG stream as `id=0x1400`, codec unknown (as for any IG
  stream). ffmpeg's `bluray:` protocol cannot open the menu discs because libbluray hides playlists
  under 3 minutes (same limit as in §5.3 for the UDF check) – VLC (libbluray with navigation) was used.

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

- ✅ **CLI `burn` (2026-10-15, no disc written):** `burn --help`; `--speed 99` rejected by
  clap (exit 2); a folder without `VIDEO_TS.IFO` → error, exit 1; `--drive Z:` on a real
  DVD folder → "Laufwerk 'Z:' nicht gefunden" with the drive list (`--drive F:`), exit 1, no
  `.iso` left behind. The happy path was **not** run because a blank DVD+R DL was in F:.

- ✅ **tsMuxeR experiment (2026-10-15, plan step 5):** two tiny MKVs concatenated with
  `"a.mkv"+"b.mkv"` → one m2ts (18.05 s = sum), one `00000.mpls`; `--custom-chapters` with two
  times → exactly two playlist marks (their PTS start at 600 s: tsMuxeR's 10-minute offset, the
  differences are the chapter distances); an **SRT placeholder** (`1 / 00:00:00,000 --> 00:00:00,040 / " "`)
  for the title without subtitles and a **PGS placeholder** (`subpic::empty_pgs`, one empty display
  set) next to a real `.sup` (taken from the m2ts) were both accepted – no fallback to dropping
  subtitle tracks was needed. Track numbering in the meta file: video `track=1`, audio `track=2+k`.
  tsMuxeR 2.7.0 creates `BDMV` only, no `CERTIFICATE`.
- ✅ **CLI Blu-ray build (2026-10-15):** `build-dvd --disc bd25 --input a.mkv b.mkv --fast`
  (12 s 720p 5.1 + stereo + SRT, 6 s 4:3): 1080p25 "als 1080i markiert", one playlist, filler
  track/placeholder messages in the report, 52 MB, ~26 s.

### 5.3 Never exercised

- 🟡 **Burn Studio M4 (2026-10-24):** nothing of the window or the operations layer ever ran against a disc: `studio::write*`/`read`/`verify`/`discovery`, the graph capture of a real operation (the `.ibg` written by `save_graph`, samples from IMAPI events), copies with the disc change, the user layer break through the dialog, on-the-fly build while burning, multi-disc `read-disc`/`verify-disc`/`drive-info` through the CLI. The Tauri side was only compiled: no command was called from a running window, the `burn-studio` capability set was never loaded by Tauri (a missing permission would only show up at run time), `--burn-studio` (hidden main window, exit when the studio closes) and "Brennstudio …" (create/focus) were never started, the file/folder dialogs and the save path of the settings dialog never clicked, the other tool modals (label, IFO, layout files, drive info, auto speed) and the layer-break modal never opened. `on_complete` shutdown/hibernate/standby never executed. Windows hibernate/standby commands are from memory. Stored-only settings are listed in the `burner/studio.rs` row of §3.1.
- 🟡 **Burn Studio M3b (2026-10-24):** `mmcwrite.rs` has only ever talked to a scripted fake drive; **no write command was sent to a real drive** (needs blank media and the user's explicit go). Unverified on hardware: every MMC byte layout listed in its §3.1 row (mode page 05, FORMAT UNIT header flags and format types, SEND DISC STRUCTURE 0x20/0x22/0x23, CLOSE TRACK number), SET STREAMING after the byte fix, OPC, RESERVE TRACK, READ FORMAT CAPACITIES capacity reading on blank media, ImapiEx exclusive access during a direct write, cancel behaviour on a half-written DVD+R (left open, not closed), speed → kB/s conversion in `burn_direct`. Planned hardware order: DVD-R test mode, then DVD+R, then BD-RE format.
- 🟡 **Burn Studio M3a (2026-10-24):** `burn_source` never burned anything (no write was run): the ring/producer logic is tested without IMAPI, but IMAPI pulling from a non-seekable `IStream` (it may `Seek`/`Stat` in ways the position-only `Seek` refuses), the telemetry from real write events, `TrayLock`/`StandbyGuard` during a real burn, the verify-by-source path against a written disc, and the tray IOCTLs (eject/load) were not run. With a disc in the drive: `drive_info` parsers (media ID offsets, book type, layer break, write speeds) are checked against synthetic buffers only – the offsets are taken from the MMC/dvd+rw-tools layout from memory and are **unverified**. DVD-R/RW media IDs are not implemented. The `.ibg` files lack ImgBurn's checksum line; no ImgBurn/third-party tool opened one.

- 🟡 **Burn Studio M2 (2026-10-24):** nothing here ever touched a drive or a disc. The reader/verifier ran against `FakeDisc` only; `stream_image`/`StreamSource` is not yet connected to a burner; Joliet names were checked through our own reader and (via the UDF side) Windows, not with `isoinfo`/`7-Zip`/Linux; the tree writer with ≥ 2^32 sectors or > 5 GiB of real data was only planned, never written; dedupe was run on small files; on-the-fly building while burning (ring buffer fill during a real write) is untested.

- 🟡 **Burn Studio M1 (2026-10-24):** a patched label was never checked by Windows (`Mount-DiskImage` after
  `patch_label`) or by a player; the `.mds` layout against Alcohol/Daemon Tools; the layer-break
  candidates on a real authored DVD (only synthetic IFOs); Joliet exists only as the patch case (our
  writer does not produce Joliet yet, M2).

- 🟡 **Blu-ray menus on anything but VLC/libbluray:** a standalone player (stricter IG timing: the
  ICS/ODS/PDS PTS are all 0 = clip start, DTS 0; real discs use a decoding lead), a real BD burn with
  menus, the Windows 10/11 players. The patched files follow libbluray's parsers and tsMuxeR's own
  output, not a BD-ROM spec document. The audio-switch button, auto-start and the sequential/loop options
  were not played (unit-tested commands only). Menu clips are encoded with x264 `veryfast` at a fixed
  8 Mbit/s whatever the frame rate/1080i flag (not checked against the BD buffer model on hardware).
- 🟡 In the **running GUI**: editing on the 16:9 stage (drag/resize, text sizes vs the build), switching
  DVD ↔ BD with separate layouts, and a build started from the GUI with a BD layout (the engine path was run via
  the CLI and `tests/bluray.rs`; the GUI only up to "Übernehmen", see §5.2).

- 🟡 **NVENC with players / more titles:** the NVENC stream was verified with ffprobe, tsMuxeR and
  libbluray (2026-10-17, driver 617.42), but never on a standalone player. libbluray prints
  "no timestamp for SPN 0" for the NVENC m2ts (not compared against an x264 build). The 4-title
  parallel test showed no speed-up from `--jobs 4` over 1 (47 s vs 45 s); 5–8 concurrent NVENC
  sessions and the GeForce session limit were not tried.
- 🟡 **Blu-ray on real hardware:** writing a BD-R/BD-RE with IMAPI2 (media probe, BD-RE overwrite,
  speeds in BD x), reading the disc back (verification), playback on a standalone player /
  VLC, burn ETA accuracy for BD. Needs a blank BD-R and the user's go.
- 🟡 Blu-ray with a **real PGS source subtitle** (only the SRT path and the placeholder PGS ran;
  the extraction code is the DVD one), Blu-ray input with subtitles of different kinds per
  title (text vs. PGS → placeholder + warning), 2-pass and hardware decoding for BD,
  interlaced and anamorphic sources, film-rate (23.976) projects end to end (only unit-tested),
  projects longer than a few minutes except the 200 s flat test clip, and a BD-50 build.
- 🟡 `udfinfo` (WSL) on a BD image; the BD image on the Unix burner (`unix.rs` compiles for
  Linux but has no BD media detection).

- 🟡 Erasing DVD-RW / overwriting DVD+RW.
- 🟡 Setting the layer break on a real DVD+R DL (SEND DVD STRUCTURE 0x20).
- 🟡 The Unix burner at runtime.
- ✅ **Button colours and word wrap (2026-10-15):** CLI build with a layout of 3 text buttons
  (default yellow/orange, green/dark green, blue/magenta) and a wrapped heading; a script read
  the NAV PCI and the IFO palette of the finished DVD: colour group 1 maps pixel 1/2/3 to the
  three pairs; the rendered menu frame shows the heading in 3 lines and the highlight mask one
  colour per button (inspected visually). Highlights on a real player were not watched.
- 🟡 Menu editor window and multi-select/groups/layer list (2026-10-17) in the running GUI (see the changelog for the checklist) – compiled only.
- 🟡 Menu editor additions of 2026-10-15 in the running GUI (wrap checkbox and engine line
  breaks, "Eigene Farben", pixel-shape previews of pictures/videos/backgrounds) – compiled only.
- 🟡 Bitmap subtitles from real Blu-ray PGS (only the test's own `.sup` and ffmpeg's decoder as
  reference), VobSub from Matroska with `.idx` palette (parsed in unit tests only), playback of
  converted subtitles on a hardware player.
- 🟡 IMAPI write events during a real burn (progress follows the written LBA, finalizing
  phase, cancel via `CancelWrite`); without events the stream fallback is used as before.
- 🟡 CLI/GUI burn on real hardware after the refactor to `burner::burn_video_ts` (the flow is
  tested with a fake burner): media checks and `--erase` in
  the CLI, real write + verify, Ctrl+C cancel (exit 130), the GUI burn dialog end to end.
- 🟡 Fast encoding (`--fast` / "Schnelles Encoding") in a real build: only the ffmpeg
  arguments are tested; speed gain and quality loss not measured; GUI checkbox not clicked.
- ✅ **Bitmap subtitles with real material (2026-10-15):** 40 s of a ConvertX-made DVD from
  `D:\ISO\Convertxtodvd\1x01 Everything's Coming Up Goofy …\VTS_01_1.VOB` (real `dvd_subtitle`
  stream) cut with `ffmpeg -c copy -f vob` and built with the CLI: 19 pictures, "19 subtitles
  added, 0 skipped", PNGs clean (white text, black outline, 3 colours); a frame of the DVD
  rendered by ffmpeg with the subpicture overlaid shows "Like father, like son" at the same
  place and size as in the source. (A Matroska remux of that VOB failed in ffmpeg because of
  packets without timestamps – a fixture problem, not ours.) No real Blu-ray PGS file was
  available; PGS is covered by the test fixture only.
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
2. ~~IMAPI progress events~~ – implemented 2026-10-15 (M2, `DDiscFormat2DataEvents` sink with
   stream fallback). Still open: watch it during a real burn (M3).
3. **DVD-R DL layer break:** currently drive default (warning shown). Would need format 0x21/
   0x22 handling or IMAPI-specific support.
4. ~~Bitmap subtitles~~ – done 2026-10-15 (M4: PGS + VobSub, own decoders). Open: a real
   Blu-ray PGS sample; DVB/XSUB still skipped; semi-transparent edges become hard (on/off).
5. ~~Unsupported settings~~ – done 2026-10-15 (M5): Half D1/SIF and SP/MP/LP implemented.
   Open: compare the profile ceilings with ConvertX's real behaviour if reference values turn
   up; check 16:9 at 352 width against the DVD spec (currently always 4:3).
6. ~~Encoding speed switch~~ – done 2026-10-15 (`--fast`, setting `fast_encode`). Still open:
   measure the real speed/quality difference on a long clip.
7. ~~Parallelism default mismatch~~ – done 2026-10-15 (`AppSettings` default = `default_jobs()`).
8. ~~Installer~~ – the user reported on 2026-10-15 that the installer works (not rebuilt or
   checked in this session).
9. ~~CLI `burn` command~~ – done 2026-10-15; flow shared with the GUI and tested with a fake
   burner (M1). Still open: one real burn through the CLI (hardware session, see below);
   optionally Ctrl+C handling for `build-dvd` (it has none; the cancel token exists).
10. ~~Delete the stray `output_menu/` folder; put the project under git~~ – done 2026-10-15.
11. ~~Custom menus: word wrap, per-button highlight colours, square-pixel previews~~ – done
    2026-10-15 (M7). Still open: test the navigation and highlights on a real player (or a
    DVD VM such as libdvdnav); click the new editor controls in the GUI.
12. ~~Blu-ray output~~ – done 2026-10-15 (BD-25/BD-50, H.264, tsMuxeR, own UDF 2.50 writer,
    menu-less autoplay; §2.8). Still open: a real BD burn + playback test (needs a blank BD-R and
    the user's go), a real PGS source, measuring x264 `slow` speed (the fast switch uses `veryfast`).
    BD menus: ~~HDMV/IGS from the menu editor~~ done 2026-10-17 (§2.8); open: a standalone player,
    IG timing with a decoding lead, button sounds/animation, multi-page menus, a pop-up menu.
13. **Blu-ray polish ideas:** show the harmonised track layout in the GUI before converting
    (today a title with fewer tracks silently gets filler tracks – only the build report says so);
    per-title text-subtitle styling; DTS/E-AC-3 pass-through instead of AC-3 re-encoding.
- 🟡 Try NVENC BDs on a standalone player; more than 4 concurrent sessions (§5.3).
- [in progress] Burn Studio (ImgBurn replacement: window, 6 modes, tools, MMC layer, graph data) → docs/plans/2026-10-24-burn-studio.md. **Done: Step 0, M1, M2, M3a, M3b and M4** (M4 = `burn-studio` window, Tauri commands/events, operations layer `burner/studio.rs`, CLI subcommands; backend `direct` → `mmcwrite::burn_direct` is wired; 🟡 never run with a disc or in a real Tauri window). **Next: M5** (full verification, STATUS §2.9, remove the plan file), then the hardware session (read-only probes, read/verify of a pressed DVD, IMAPI write, direct engine). Known gaps of M4: no drag & drop into the file list, no per-file destination editing (`BuildInput.sources[].dest` is always empty), no batch mode ("eject and next disc" for reading), the ignored settings listed in §3.1. The plan file stays until M5; resume at the first unticked item of its checklist.

Agreed order of the remaining work (2026-10-15 audit; details in the review plan):
M1 shared burn flow + fake-burner tests + process hygiene (**done**) → M2 IMAPI write events
(#2, code + mapping tests; **done**) → M3 hardware session (#1 + real CLI/GUI burn + M2 events; needs
discs and the user's go; postponed) → M4 bitmap subtitles (#4, pure-Rust PGS/VobSub decoding →
spumux; **done**) → M5 Half D1/SIF + SP/MP/LP (#5; **done**, profile ceilings as
proposed) → M6 installer (#8; dropped – works per user) → M7 custom-menu polish (#11; **done**).
#3 (DVD-R DL layer break) is parked.

### 6.2 Not implemented at all (out of current scope)

- ⛔ BD-J, Blu-ray pop-up menus, ConvertX templates on Blu-ray, AVCHD on DVD, HEVC/UHD output, custom size for BD, multi-session. (NVENC H.264 for Blu-ray exists, §2.8.) (Plain autoplay Blu-ray with H.264 exists, §2.8.)
- ⛔ LPCM / MP2 / DTS / TrueHD / E-AC-3 audio output (AC-3 only, also on Blu-ray).
- ⛔ Multi-angle, seamless branching, DVD-Audio, CSS, region coding.

### 6.3 Known limitations, edge cases & caveats

- **Blu-ray:** one frame rate for the whole disc (a 24p film next to a 25p video makes both 1080p25
  flagged-interlaced); the BD spec has no 1080p25/29.97, hence `fake-interlaced`. Titles with fewer
  audio/subtitle tracks than others get silent AC-3 / empty subtitle tracks (position-wise), a
  subtitle of another kind (text vs. PGS) at the same position is dropped with a warning.
  VobSub/DVB/XSUB/teletext subtitles are skipped with a warning. Text subtitles are rendered by
  tsMuxeR (Arial, fixed size/position). Chapter marks and subtitles depend on `apad` + `-t`
  keeping every stream of a title the same length. Playlist marks start at tsMuxeR's 600 s offset.
  Without a layout the disc has no menu: playback starts the playlist at once. **With a layout** (menus):
  HDMV only; DVD and BD layouts are separate (a 4:3 layout cannot be converted sensibly); the IGS is
  muxed as a disguised PGS track and patched afterwards, which depends on tsMuxeR 2.7.0's output
  layout; every `SetStream` call rewrites the PG display flag (the audio buttons re-read PSR2 first);
  a motion menu restarts its clip after each loop (the IG epoch restarts with it); the Top Menu / Menu
  key leads to the first title menu (there is no per-title popup menu); menu clips are re-encoded, so
  the bitrate plan reserves their real size; never tested on a standalone player. The work directory's `title_NN.mkv`
  files are as large as the final disc (kept with `--keep-work`). Hardware H.264 encoders are not
  used (no reliable BD-compatible HRD). `Custom(mb)` stays DVD-only. The image of a BD has no ISO 9660
  side and aligns every file to 32 sectors (a few hundred KB of padding). Quality thresholds for
  the traffic light are 15,000 / 8000 kbps on BD.

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
- **Bitmap subtitles:** 3 opaque colours per picture (k-means) and hard transparency; very
  colourful or gradient subtitles lose detail. Pictures shorter than 40 ms are dropped, overlaps
  are cut at the next start, a never-cleared last PGS picture lasts 5 s. VobSub without a palette
  (raw VOB input) gets white/black/grey. Subtitles in the source's letterbox bars are kept inside
  the frame. All pictures of a movie are written as PNGs into the work directory (a few thousand
  small files). spumux places subpictures on frame boundaries (±1 frame).
- **Half D1 / SIF / profiles:** reduced resolutions force 4:3 for all titles (one warning lists
  wide sources). SP equals Auto in practice. MP/LP make short projects smaller than the disc.
  LP's switch to Half D1 depends on the final bitrate, which includes the real menu size. The
  LP integration test relies on a 13 MB custom target landing between 1000 and 3000 kbps.
- **Settings file** compatibility is forward/backward tolerant, but unknown enum values are
  reset to defaults on load. Settings files saved before 2026-10-15 keep their stored
  `max_concurrent_encodes` (typically all CPUs); there is no migration because a stored value
  cannot be told apart from a deliberate choice – "Standardwerte" in the dialog applies the new
  default (half the CPUs).
- **CLI `burn`:** `--erase` is required for rewritable discs with content (no prompt);
  `--keep-iso PATH` overwrites an existing file at PATH once the new image is complete (a
  failed or cancelled image phase leaves it alone; a failed burn afterwards removes the new image); Ctrl+C during the IMAPI write phase
  leaves a write-once disc unusable (same as the GUI cancel).
- **Custom layouts:** coordinates are TV storage pixels (720 wide, non-square); the editor shows
  the stage at 4:3. Pictures (contain/cover) are fitted with the TV pixel shape since
  2026-10-15 – existing layouts therefore look ≈ 6 % narrower (PAL) than before, now correct on
  the TV; "stretch" fills its box as before. Texts are still drawn in storage pixels (glyphs as
  wide as the stage shows them). Without `wrap`, overlong lines end with "…" in the engine and
  are clipped in the editor; with `wrap` the editor shows the engine's lines (until they
  arrive, the browser's own wrapping). With 2–3 button colour pairs in a menu the highlight
  edges are hard (no half-transparent edge pixel left). Image paths
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
convertx-rust build-dvd --disc bd25 --input a.mkv b.mp4 --output D:\out --resolution hd1080 --fast   # Blu-ray: no --template needed, one autoplay playlist
convertx-rust build-dvd --disc bd25 --input a.mkv b.mp4 --layout bd-menu.json --output D:\out --resolution hd720 --fast   # Blu-ray with menus (16:9 layout, canvas 1920x1080)
convertx-rust burn D:\out\Film --keep-iso            # project folder, VIDEO_TS or BDMV
convertx-rust drives
convertx-rust make-iso D:\out\Film\VIDEO_TS D:\out\Film.iso --label "Mein Film"
convertx-rust burn D:\out\Film --drive F: --speed 8 --keep-iso   # Ctrl+C cancels; exit 130
convertx-rust build-dvd --input a.mkv --template "…\10_TitlesPerPageNoRoot.ini" --output D:\out --fast  # draft quality
convertx-rust build-iso D:\Daten --output D:\daten.iso --label MEINE_DATEN   # Burn Studio: UDF + ISO 9660 + Joliet image (--as-disc for a VIDEO_TS/BDMV folder)
convertx-rust iso-label D:\daten.iso --set NEUES_LABEL
convertx-rust ifo-layerbreak D:\Film\VIDEO_TS         # candidate table for a DL layer break
convertx-rust read-disc D:\disc.iso --drive F:        # also: verify-disc [IMAGE], drive-info
convertx-gui.exe D:\out\Film\VIDEO_TS        # opens the burn dialog for an existing DVD folder
convertx-gui.exe --burn-studio               # starts with the Burn Studio window only
```

### Conventions

- UI text and user-facing errors are **German**; code, comments and identifiers English.
- Comments explain *why*; match the surrounding density.
- Every external process goes through `tools::tool_command` (or `Toolchain::command`), or
  `tools::blocking_tool_command` in synchronous code; `tests/process_hygiene.rs` fails on any
  other `Command::new` in `src/`/`src-tauri/src/`. Integration tests may spawn tools directly.
- Burning goes through `burner::burn_video_ts` (GUI and CLI); test new flow logic with a fake
  `OpticalBurner` like `tests/burn_flow.rs`.
- Integration tests must skip gracefully when tools/templates are missing.
- Patch scripts run through Bash heredocs can lose backslashes (regexes, `\n`, Windows
  paths) — write such scripts to a file first.

### Key facts that are easy to get wrong

- A file ffmpeg still holds open shows **0 bytes** in Explorer / `Get-ChildItem` (NTFS updates the
  directory entry on close). Measure with an open handle (`FileStream.Length`). "0 % CPU / 0 % GPU"
  during a BD encode usually means x264 `slow` is simply CPU-bound (~0.45× real time for 1080p).

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

### 2026-10-24 — Burn Studio, M4 (Tauri commands/events, operations layer, CLI subcommands, `burn-studio` window)

- Part of `docs/plans/2026-10-24-burn-studio.md` (**not finished**: M5 open, plan file kept, M4 ticked). New `src/burner/studio.rs` (operations shared by GUI and CLI: write image/files, Discovery, read, verify, build, auto-speed rules, backend dispatch IMAPI `burn_source` vs `mmcwrite::burn_direct`, telemetry → graph events + `.ibg`, copies, on-complete action), `src/studio_cli.rs` (`read-disc`, `verify-disc`, `build-iso`, `iso-label`, `ifo-layerbreak`, `drive-info`), `src-tauri/src/burn_studio.rs` (commands + events `studio-progress`/`studio-graph`/`studio-log`/`studio-done`, one operation at a time), the window (`frontend/burnStudio.html`, `src/burnStudio.ts`, `src/studio/{api,ui,views,graph,settings,tools,layerbreak}.ts`, Vite input `studio`), `--burn-studio` start flag, capability for the `burn-studio` window, "💿 Brennstudio …" button in the main window. Settings the engine does not act on are shown disabled with the reason.
- Files: new `src/burner/studio.rs`, `src/studio_cli.rs`, `src-tauri/src/burn_studio.rs`, `frontend/burnStudio.html`, `frontend/src/burnStudio.ts`, `frontend/src/studio/*.ts`, `tests/studio_cli.rs`; changed `src/main.rs`, `src/burner/mod.rs`, `src-tauri/src/main.rs`, `src-tauri/capabilities/default.json` (+ regenerated `gen/schemas/capabilities.json`), `frontend/index.html`, `frontend/src/main.ts`, `frontend/src/styles.css`, `vite.config.ts`, `STATUS.md`, plan checklist.
- Verified: `cargo test --workspace` 338/338 (7 new: 3 unit, 4 CLI), `cargo check --workspace --all-targets`, Linux `--lib --tests` check and `npm run build` green; the studio page ran in the built-in browser with a mocked `invoke` (§5.2). **Not verified:** any operation with a disc, any Tauri command from a real window, the capability set, `--burn-studio`, file dialogs, the other modals (§5.3).

### 2026-10-24 — Burn Studio, M3b (direct MMC write engine "SPTI Direkt")

- Part of `docs/plans/2026-10-24-burn-studio.md` (**not finished**: M4–M5 open, plan file kept).
  New `mmcwrite.rs`: `burn_direct_with` over any `ScsiTransport` (format, mode page 05, streaming,
  OPC, layer break, reserve track, WRITE(10) loop from the ring buffer with retries and device
  buffer telemetry, sync, close, verify, eject), overburn policy, `DirectOptions::from_settings`,
  Windows `burn_direct` (SPTI or IMAPI-Ex with exclusive access). Fixed `spti::set_streaming()`
  (type/length bytes). Not yet wired into `OpticalBurner`/settings (M4).
- Files: new `src/burner/mmcwrite.rs`; changed `src/burner/{mod,spti,windows}.rs` (module line,
  set_streaming fix + test, `volume_paths` visibility), `STATUS.md`, plan checklist.
- Verified: `cargo test --workspace` 331/331 (14 new, all with a fake drive; no real drive was
  sent any command), `cargo check --workspace --all-targets`, Linux `--lib --tests` check and
  `npm run build` green. Not verified: everything on real hardware, see §5.3.

### 2026-10-24 — Burn Studio, M3a (MMC layer, drive info, power/tray guards, `burn_source`, graph telemetry)

- Part of `docs/plans/2026-10-24-burn-studio.md` (**not finished**: M3b–M5 open, plan file kept).
  New `spti.rs` (CDB builders, parsers, `ScsiTransport` with `ImapiExTransport` + `SptiTransport`,
  read-only `drive_info`), `power.rs` (standby guard, tray/lock via storage IOCTLs), `graph.rs`
  (sampler, file-name template, `.ibg` writer/parser). `OpticalBurner::burn_source` (IMAPI only)
  burns any `SectorSource` through a ring buffer; `burn_iso` now shares its implementation
  (`burn_feed`) with an unchanged file feed. `BurnOptions` gained ring/recovery/standby/tray/telemetry
  fields (existing literals got `..Default::default()`).
- Files: new `src/burner/{spti,power,graph}.rs`; changed `src/burner/{mod,windows,source}.rs`,
  `src/main.rs`, `src-tauri/src/burn.rs`, `tests/burn_flow.rs` (only the `BurnOptions` literal),
  `Cargo.toml` (windows features).
- Deviations from the plan: DVD-R/RW media ID not implemented; `burn_source` always uses a ring
  (a source with its own ring is read directly); copies / delete-after / auto speed rule are left
  to the M4 operation layer (they are loops around `burn_source`, not part of one burn);
  `burn_source` exists for IMAPI only (the Unix burner reports "nicht unterstützt").
- Verified: `cargo test --workspace` 317/317, `cargo check --workspace --all-targets` and the
  Linux `--lib --tests` check clean, `npm run build` green; read-only host probe on the real drive
  (§5.2). Not verified: any write through `burn_source`, see §5.3 (Burn Studio M3a entry).

### 2026-10-24 — Burn Studio, M2 (generic UDF/ISO/Joliet tree writer, builder, sources, ring buffer, reader, verifier)

- Part of `docs/plans/2026-10-24-burn-studio.md` (**still not finished**: M3a–M5 open, plan file kept).
  `udf.rs` now has one generic writer (`plan_tree` / `write_to`); `plan_image` is a wrapper and the
  DVD-Video image stayed byte-identical (golden hash from step 0, plus the Windows mount test).
  New: Joliet, level-1 ISO names, multi-extent files, ISO area behind the anchor, dedupe, minimum
  size/start sector, any `Write` sink for both UDF writers (`write_to`, `write_bd_to`), a
  per-file `Transform` hook (CPI reset), `builder.rs`, `source.rs`, `ringbuf.rs`, `reader.rs`,
  `verifier.rs`, `verify_against_image` as a wrapper of the verifier.
- Files: new `src/burner/{builder,source,ringbuf,reader,verifier}.rs`; changed `src/burner/{udf,udf250,
  layerbreak,mod,isoread,tools}.rs` (`TreeEntry` literals → constructors), `tests/disc_image.rs`.
- Deviations from the plan: no `iso_level1_names` option (always level 1; Joliet carries long
  names); `ifo_bup_ecc_padding` and `read_dvd_text` are stored settings without effect; the Discovery
  test source is `source::discovery` (zeros, size ≤ free capacity).
- Verified: `cargo test --workspace` 288/288 (45 new; the count includes one test from a parallel
  session), `cargo check --workspace --all-targets` and the Linux `--lib --tests` check clean,
  `npm run build` green; the data-tree image mounts in Windows (`Mount-DiskImage`) with all files
  identical. Not verified: see §5.3 (Burn Studio M2 entry).

### 2026-10-24 — Burn Studio, Step 0 + M1 (settings, sidecars, ISO reader, label patcher, IFO inspector)

- Part of `docs/plans/2026-10-24-burn-studio.md` (**not finished**: M2–M5 open, plan file kept).
  Step 0: checkpoint commit of the menu-editor window work; `md-5` crate; golden-hash test for the
  VIDEO_TS image. M1: `AppSettings.burn_studio` (all sections of the plan), `.dvd`/`.cue`/`.mds`
  files, ISO 9660 reader, label read/patch (DVD + BD images), layer-break candidate walk.
- Files: new `src/burner/{sidecar,isoread,tools,testutil}.rs`; changed `src/config.rs`,
  `src/burner/{mod,layerbreak}.rs`, `Cargo.toml`/`Cargo.lock`, `tests/disc_image.rs`.
- Deviations from the plan: settings choices are strings validated by `sanitized()` (house style)
  instead of serde enums; the tools.rs file-writer wrappers were left out (`sidecar::write_*`).
- Verified: `cargo test --workspace` 243/243 green (15 new), `cargo check --workspace --all-targets`
  and `cargo check --lib --tests --target x86_64-unknown-linux-gnu` clean; frontend untouched
  (`npm run build` ran green before M1). Not verified: see §5.3 (Burn Studio M1 entry).

### 2026-10-24 — Menu highlights: icon buttons with captions outside the button rectangle

- Bug: on the Metallic (open style) root menu the nav buttons (Resume, Chapters, Settings) light
  up in VLC but not on real players. Cause: the button's caption (`GT_TEXT`, same `Location`) was
  used as highlight mask, but it sits below the button rectangle (e.g. caption top 501, button
  381–501); the later "clear everything outside the spumux rectangles" step left an **empty mask**.
- `src/menu.rs` (`render_page`): a text mask with no pixels inside the rectangle now falls through
  to the overlay-GIF / bitmap mask; if that is empty too, the whole rectangle is used, so no button
  can end up without highlight pixels. Buttons whose text lies inside the rectangle are unchanged.
- `tests/render_menu.rs`: `icon_buttons_with_outside_captions_light_up` (Open_Style.ini page 1).
- Verified: swept all 489 buttons of every installed template (pages 0–7, CLI `render-menu`) –
  0 empty highlight masks after the fix (page 1 of Open_Style was empty before); `render_menu`
  (3), `custom_menu` (5), `motion_menu` (7) integration tests green. `cargo test --lib` could not
  be linked (test exe locked by a parallel session) – not re-run; full suite count not refreshed.
- **Not tested on a real player.** Existing discs must be re-authored.

### 2026-10-24 — DVD+R DL: no forced media close (IMAPI "Featureseite nicht aktuell")

- A user's DVD-9 burn on DVD+R DL failed at ~94 % with `IMAPI2: Die angeforderte Featureseite wird
  unterstützt, sie ist jedoch nicht als aktuell gekennzeichnet (0x80004005)`. Hypothesis (not
  proven): the final forced close (`SetForceMediaToBeClosed`) is what the drive rejects; the
  progress value runs ahead of the real write, so 94 % = closing phase.
- `src/burner/windows.rs`: `SetForceMediaToBeClosed(true)` is skipped for `MediaType::Dvd9`.
- Verified: `cargo check --lib` only. **Not tested on a disc** – needs a DVD+R DL burn; the
  disc may now stay appendable instead of finalised (player compatibility unchecked).

### 2026-10-17 — Menu editor in its own window; multi-select, groups, layer list

- Plan `docs/plans/2026-10-17-menu-editor-window-multiselect.md` implemented (and deleted).
  **Window:** `editor.html` + `editorWindow.ts`, data exchange by events (`editor-ready` →
  `editor-init`, `editor-apply`), "Übernehmen" no longer closes, "Abbrechen" → "Schließen",
  window close asks when unapplied; capability `default.json` for `menu-editor` (create window,
  focus, close, destroy); main window close destroys the editor. **Editor:** selection is a set
  (Shift+click, marquee, Alt+click single), group move clamped on the bounding box, group resize
  per element, Ctrl+G/Ctrl+Shift+G, Ctrl+C/X/V/D, arrows/Delete on the selection, layer list.
  `Element.group` (Rust + TS), unit test for the round trip.
- Files: new `frontend/editor.html`, `frontend/src/editorWindow.ts`; changed `menuEditor.ts`,
  `main.ts`, `layout.ts`, `styles.css`, `vite.config.ts`, `src-tauri/capabilities/default.json`,
  `src-tauri/src/main.rs`, `src/{layout,bd_menu}.rs`, `tests/bluray.rs`.
- **Verified:** `npm run typecheck`, `npm run build` (`dist/editor.html` present), `cargo check -p
  convertx-gui`, `cargo test --workspace` **228/228** (+1). **Not verified:** nothing was clicked in
  the GUI (window opening/focus, events, close confirm, dialogs in the second window, `tauri dev`
  URL of `editor.html`, all Part B interactions, Alt+click menu-focus side effect, layout with groups
  save/load); the plan's manual checklist is still open. Capability names were accepted by the build only.

### 2026-10-17 — Blu-ray menus from the menu editor (HDMV navigation + Interactive Graphics)

- Plan `docs/plans/2026-10-17-bluray-menus.md` implemented (and deleted). New: `src/igs.rs`,
  `src/hdmv.rs`, `src/bd_menu.rs`, `src/bd_patch.rs`, `examples/igs_spike.rs` (the spike, kept as a
  hand-built reference disc generator). Changed: `author.rs` (`MenuFrame`), `layout.rs`
  (`validate_bd`, frame-generic helpers), `encoder.rs` (`custom_menu_graph`, `bd_custom_menu_args`,
  `menu_pixels_for`; DVD arguments unchanged), `bd.rs` (menu stage, per-menu / per-title tsMuxeR runs
  with offsets, `write_navigation`, menu size in the bitrate plan), `bd_author.rs` (per-title and
  menu meta), `main.rs` (CLI summary / help), `tests/bluray.rs` (`mod menus`), Tauri
  `menu_editor.rs` (`BD1080`/`BD720` frame names) and `main.rs` (`bdMenuSecs`), frontend `main.ts`
  (menu slot per medium, frame name, BD menu tab), `layout.ts` (16:9 canvas + `toBluray` draft),
  `menuEditor.ts` (16:9 stage, square pixels, 255 buttons), `api.ts`.
- Key findings: tsMuxeR drops IGS segments but copies PCS-labelled ones byte-identically → disguise +
  patch (`bd_patch`); libbluray's `SetStream` writes the PG display flag from bit 14 of every call;
  a compare skips the next command when false; libbluray (ffmpeg `bluray:`) hides playlists < 3 min.
- **Verified:** `cargo test --workspace` **227/227** (was 208: +17 engine unit, +2 `tests/bluray.rs`);
  `cargo check --workspace --all-targets` and `cargo check --lib --tests --target
  x86_64-unknown-linux-gnu` clean; `npm run build` clean; VLC playback of a spike disc and of a full CLI
  build (§5.2): menus render, navigation, chapter jump, play-all, title end → menu, motion-menu
  selection across loops, subtitle switch. **Not verified:** standalone players, audible audio switch,
  auto-start / sequential / loop options on a player, GUI editing on the 16:9 stage and a GUI-started BD menu build (§5.3).

### 2026-10-17 — NVENC hardware H.264 encoding for Blu-ray (default on, x264 fallback)

- `encoder.rs`: `BdEncoder`, `BdEncodeJob::{encoder, nvenc_args, x264_two_pass}`, `nvenc_bd_options`;
  `run_bd_encode` skips the analysis run for NVENC. `tools.rs`: `probe_nvenc`,
  `nvenc_failure_reason`. `bd.rs`: probe once per build, 1080i → x264, `NVENC_MAX_JOBS = 4`, ETA
  passes, Step/warning. `dvd.rs`/`config.rs`: `bd_hw_encode` (settings default true). `main.rs`:
  `--no-hw-encode`. Frontend: checkbox in the Blu-ray group (`settings.ts`, `api.ts`).
  `tests/bluray.rs`: `hw_encode_uses_nvenc_or_falls_back`.
- Background: the reported "stall" was not a deadlock – x264 `slow` at 100 % CPU (see §7).
- **Verified:** `cargo test --workspace` 208/208 green; `tsc` clean. With driver 591.86 only the
  fallback ran (warning "NVIDIA-Treiber zu alt", x264 result High@4.1). After the driver update
  (617.42): `hw_encode_uses_nvenc_or_falls_back` passes on the NVENC branch; a real 2-min Handy
  Manny clip through `build-dvd --disc bd25` used NVENC: ffprobe h264 High level 41 1920×1080
  24000/1001 `has_b_frames=2`, tsMuxeR accepted it, libbluray read a 210 s build (h264 + ac3);
  whole build 12 s vs 72 s with `--no-hw-encode` (x264 `slow`, ~6×). **Not verified:** standalone
  players, >4 concurrent sessions (§5.3).

### 2026-10-15 — Blu-ray output (BD-25 / BD-50): H.264, menu-less autoplay, own UDF 2.50 writer

Implemented `docs/plans/2026-10-15-bluray-support.md` (the plan file is deleted; its decisions
are in §2.8). Before that, the working tree of M4/M5/M7 was committed as a checkpoint
(`0d4bea2`); `bin/tsMuxeR.exe` + `bin/tsMuxeR-LICENSE.txt` are staged, nothing else is committed.

- **Engine:** `DiscType::{Bd25, Bd50}` with `limits()` (4000–35,000 kbps, ≤ 48,000 total, quality
  15,000 / 8000), 6 % mux overhead, profile ceilings per disc (`calculator.rs`);
  `Resolution::{Hd1080, Hd720}` (`author.rs`); new `bd.rs` (orchestrator `build_bd`,
  `bd_frame_rate`, track harmonising), `bd_author.rs` (tsMuxeR meta file), `BdEncodeJob` +
  `run_bd_encode` + shared `extract_subtitle_args` (`encoder.rs`), optional `Toolchain::tsmuxer`
  (`tools.rs`), `subpic::empty_pgs`, `dvd::build_dvd` dispatch + `pub(crate)` helpers +
  `bd_single_video` + HD-on-DVD check (`dvd.rs`), `eta::Medium`, settings `bd_resolution` /
  `bd_single_video` (`config.rs`).
- **Image/burn:** new `burner/udf250.rs` (UDF 2.50 with metadata partition, planned then
  written); `udf.rs` helpers made `pub(crate)` (+ `tag_v`, `NotBluray`); `burner/mod.rs`:
  `MediaType::{Bd25, Bd50}`, `DiscContent`, `is_bdmv_folder`, `bd_media_type`, BD branch in
  `burn_video_ts` (no layer break), `media_problem(media, &content)`, speed helpers per medium,
  `BurnEta` per medium; `windows.rs`: BD-R/BD-RE media.
- **CLI/GUI:** `--disc bd25|bd50`, `--resolution hd1080|hd720` (default per disc),
  `--single-video`, `--template` optional for BD, `burn` accepts BDMV/project folders, `bitrate`
  prints the limits; Tauri `calculate_project`/`disc_needs`/`media_view`/delete rules/
  `startup_burn`; frontend disc picker, BD tab text, settings group, burn dialog wording;
  `tauri.conf.json` bundles the tsMuxeR licence.
- **Tests:** +37 (engine units 113 → 139, GUI units 10 → 13, integration 45 → 53: new
  `tests/bluray.rs` with 4, `burn_flow.rs` +4 Blu-ray tests and 2 adjusted messages), see §5.1.
- **Verified:** `cargo test --workspace` 205/205 green; `cargo check --workspace --all-targets`
  and `cargo check --lib --tests --target x86_64-unknown-linux-gnu` without warnings;
  `npm run build`; the Windows mount of the UDF 2.50 image and libbluray reading it (§5.1);
  the tsMuxeR experiment and a CLI build (§5.2). **Not verified:** any real BD burn/playback,
  the GUI clicked through with the new controls (compiled only), see §5.3.
- Plan deviations: see the last bullet of §2.8 (metadata file locations, `CERTIFICATE`,
  capacity constants).

### 2026-10-15 — Audit fixes (ponytail audit, findings 1–4)

- **Data loss fix:** `burn_video_ts` removed `job.iso` after any failure, even when the image
  phase failed or was cancelled before the `.iso.part` was renamed – so CLI `burn --keep-iso PATH`
  deleted an existing file at PATH that this run never wrote. Now `job.iso` is only removed when
  the image was written; `.iso.part` is always removed.
- `dvd.rs`: stream selection moved 1:1 out of `build_dvd_inner` into `select_streams` (same
  warnings) and unit-tested (language filter, preferred language first, fallback to the first
  track, user choice wins, unsupported bitmap skipped).
- Removed the unused Tauri command `sanitize_label` (the frontend uses `frontend/src/label.ts`).
- One work-directory constant: `dvd::WORK_DIR` replaces `WORK_MARKER`/`WORK_DIR_NAME` and the
  GUI's own copy in `src-tauri/src/burn.rs`.
- Files: `src/burner/mod.rs`, `src/dvd.rs`, `src-tauri/src/{burn,main}.rs`, `tests/burn_flow.rs`.
- Verified: `cargo test --workspace --lib --bins` (113 + 10 + 0 green, GUI crate compiles);
  `cargo test --test burn_flow` 9/9 green incl. new `cancelled_image_keeps_an_existing_file`.
  Other integration tests not re-run; clippy is not installed on this toolchain.

### 2026-10-15 — M7: menu editor polish (word wrap, button colours, pixel shape)
- Word wrap: `Text::wrap` (JSON `wrap`, default off; new editor texts on), `menu::text_lines`
  (greedy at spaces, long words at characters, "…" when the box is too low),
  `layout::element_text_lines`, GUI command `text_lines`; the editor shows the engine's lines.
- Button colours: `Element::highlight` (`ButtonColors`), up to 3 pairs per menu, validation
  (limit, no shared colour); masks give every pair its own pixel value (one pair keeps the
  smooth edge). Editor: "Eigene Farben" with two colour fields per button.
- Pixel shape: `TvStandard::menu_pixel_aspect`, `encoder::menu_pixels`, `layout::fit_image`
  squeeze the axis that ends up horizontal, so contain/cover pictures keep their proportions
  on the TV; the editor previews pictures, videos and backgrounds the same way.
- Files: `src/{author,encoder,layout,menu}.rs`, `src-tauri/src/{main,menu_editor}.rs`,
  `frontend/src/{api,layout,menuEditor}.ts`, `tests/custom_menu.rs`.
- Verified: `cargo check --workspace --all-targets` without warnings, Linux-target check,
  `npm run build` (tsc), `cargo test --workspace` 166/166 green (3 new unit tests, 1 new
  integration test reading the button colour table and palette back from the DVD); CLI build
  inspected (§5.2). Not verified: the new editor controls in the running GUI, highlights on a
  real player.

### 2026-10-15 — M4: bitmap subtitles (PGS, VobSub)
- New `src/subpic.rs`: PGS and VobSub decoders, ffprobe hex-dump reader, canvas → DVD frame
  mapping, 3-colour quantising, palette PNGs + spumux image XML.
- `src/encoder.rs`: `EncodeJob::subtitles` (text and bitmap in DVD order), PGS `.sup` extraction
  next to the SRTs, `bitmap_subtitle_xml` (decode/render via `spawn_blocking`); `src/tools.rs`:
  `Toolchain::ffprobe_output`; `src/dvd.rs`: only DVB/XSUB/teletext are skipped now.
- GUI: PGS/VobSub tracks selectable and preselected (`supported`), hint text, editor stream list.
- Files: `src/{subpic,encoder,tools,dvd,lib}.rs`, `src-tauri/src/main.rs`, `frontend/src/main.ts`,
  `tests/{bitmap_subs,two_pass}.rs`.
- Verified: `cargo check --workspace --all-targets` without warnings, Linux-target check,
  `npm run build`, `cargo test --workspace` 162/162 green (7 new unit, 2 new integration tests);
  real VobSub DVD material converted and checked visually (§5.2). First run of the PGS test
  found a 1 s shift that came from the fixture (ffmpeg normalises each input file's start;
  fixed with `-copyts` when muxing the test `.sup`), not from the conversion.

### 2026-10-15 — M5: Half D1 / SIF resolutions and SP/MP/LP profiles
- `author::Resolution` (Full D1, Half D1, SIF; `frame_size`, `allows_wide`); `EncodeJob::resolution`,
  `encoder::video_filter(…, resolution)`, `active_size` on the coded frame.
- `calculator::EncodingProfile` (ceilings Auto/SP 8500, MP 6000, LP 4000 kbps) via
  `calculate_capped`; `dvd::bitrate_plan(…, profile)`, `dvd::effective_resolution` (LP → Half D1
  below 3000 kbps); reduced sizes force 4:3 with one warning; step texts show profile and size;
  `BuildReport::resolution`.
- Settings `dvd_resolution`/`encoding_profile` now applied (`UNSUPPORTED_CHOICES` empty), CLI
  `build-dvd --resolution --encoding-profile`, GUI live estimate uses the profile, German
  explanations in the settings dialog.
- Installer: reported working by the user; roadmap item 8 / M6 dropped.
- Files: `src/{author,calculator,config,dvd,encoder,main}.rs`, `src-tauri/src/main.rs`,
  `frontend/src/settings.ts`, `tests/{resolution,two_pass}.rs`.
- Verified: `cargo check --workspace --all-targets` without warnings, Linux-target check,
  `npm run build`, `build-dvd --help` lists the new options, `cargo test --workspace` 153/153
  green (new: 3 engine unit, 1 GUI unit, 3 integration with real ffmpeg/dvdauthor). Not
  checked: playback of Half D1/SIF discs on a player, the settings dialog in the running GUI.

### 2026-10-15 — M2: IMAPI write events
- `src/burner/windows.rs`: `WriteEvents` sink (`#[implement(DDiscFormat2DataEvents)]`, also
  `IDispatch::Invoke` for DISPID 0x200) connected via `IConnectionPointContainer` around
  `IDiscFormat2Data::Write` (`EventConnection`, unadvised on drop). Pure `write_update` maps
  IMAPI actions to `BurnPhase` and the written LBA to the fraction; repeats are skipped;
  cancel calls `CancelWrite`. Only on an MTA thread; the image stream keeps reporting until the
  first event arrives (no frozen progress if events never come).
- Verified: `cargo check --workspace --all-targets` without warnings, Linux-target check,
  `cargo test --workspace` 146/146 green (2 new unit tests; one advises the sink on a real
  `MsftDiscFormat2Data`, no disc access). Not verified: events during a real write (M3).

### 2026-10-15 — Roadmap audit; M1: shared burn flow, fake-burner tests, process hygiene
- Audit of §6.1 against media needs and hard rules; the order M1–M7 is recorded in §6.1.
  Found: `burner/unix.rs` and the GUI's `open_folder` started processes without
  `tool_command`; §6.1 #11 listed already finished settings pages.
- `burner::burn_video_ts` (+ `BurnJob`, `BurnUpdate`, `BurnOutcome`, `BurnError::Image`): the
  image → burn → clean-up flow that GUI (`run_burn`) and CLI (`burn`) each had is now one engine
  function; GUI events/messages and CLI output unchanged apart from image errors now starting
  with "Disc-Image:".
- `tools::blocking_tool_command` (no window, stdin closed) used by the Unix burner and
  `open_folder`; new `tests/process_hygiene.rs` guards the rule.
- New `tests/burn_flow.rs` (8 tests, fake burner + CLI error paths).
- Files: `src/tools.rs`, `src/burner/{mod,unix}.rs`, `src/main.rs`, `src-tauri/src/burn.rs`,
  `tests/{burn_flow,process_hygiene}.rs`.
- Verified: baseline 135 green before the change; afterwards `cargo check --workspace
  --all-targets` and the Linux-target check without warnings, `cargo test --workspace` 144/144
  green. `cargo clippy` is not installed for this toolchain (not run). No real burn (a blank
  DVD+R DL in F: is reserved for the hardware session). The fixture DVD of `burn_flow.rs`
  stays in `%TEMP%\cx_burnflow_<pid>` (shared by the tests, no teardown).

### 2026-10-15 — Parallelism default, fast encoding, CLI `burn` (§6.1 items 6, 7, 9)
- `AppSettings::max_concurrent_encodes` now defaults to `dvd::default_jobs()` (half the logical
  CPUs) like the engine; existing settings files keep their stored value (§6.3). Settings hint
  names the default.
- Fast encoding: `EncodeJob::fast` drops `-trellis 1` (all runs); `BuildOptions::fast_encode`,
  setting `fast_encode` (default off, old files load), CLI `build-dvd --fast`, GUI checkbox
  "Schnelles Encoding (Entwurf)" under Enkodierung › Leistung; step text says "schnell".
  No `--encoding-profile fast`: `encoding_profile` is the bitrate profile (Auto/SP/MP/LP).
- CLI `burn <VIDEO_TS|project>`: `--drive` (id, letter or name part; default the only writer),
  `--speed`, `--label` (default project folder name), `--no-verify`, `--erase`, `--no-eject`,
  `--keep-iso [DATEI]`; Ctrl+C cancels via a tokio signal thread (exit 130); progress lines
  with `BurnEta` remaining time. `burner::media_problem` and `burner::default_iso_path` moved
  from `src-tauri/src/burn.rs` into the engine and are shared by GUI and CLI.
- Files: `src/{config,dvd,encoder,main}.rs`, `src/burner/mod.rs`, `src-tauri/src/burn.rs`,
  `frontend/src/{api,settings}.ts`, `tests/two_pass.rs`.
- Verified: `cargo check --workspace --all-targets` without warnings, Linux-target check,
  `npm run build`, `cargo test --workspace` 135/135 green; CLI `burn` error paths by hand (§5.2).
  Not run: a real CLI burn, a real `--fast` build, the GUI checkbox (§5.3).

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
