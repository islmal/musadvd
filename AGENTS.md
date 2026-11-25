# AGENTS.md – MusaDVD technical manual for coding agents

Self-contained guide to maintaining and extending MusaDVD without breaking hardware compatibility.
Every name below exists in the code (checked when this file was written, version 1.5.1). When this
file and the code disagree, **the code wins** – fix this file in the same change.

Companion documents: `CLAUDE.md` (working rules and shell setup), `STATUS.md` (architecture summary,
quirks, open issues, dead ends – read section 5 before debugging), `handbook/` (per-module
reference), `CHANGELOG.md` (user-visible history), `SYSTEM_STATE.txt` (generated file tree, never
edit by hand).

---

## 1. Project identity & core philosophy

**What it is.** MusaDVD turns arbitrary video files into standards-compliant optical discs and
burns them:

| Output | Disc types (`calculator::DiscType`) | Structure | Video |
|---|---|---|---|
| DVD-Video | `Dvd5` (target 4300 MB), `Dvd9` (7950 MB), `Custom(mb)` | `VIDEO_TS/` (+ empty `AUDIO_TS/` in images) | MPEG-2 + AC-3 |
| Blu-ray | `Bd25` (22,900 MiB), `Bd50` (45,800 MiB) | `BDMV/` + `CERTIFICATE/` | H.264 + AC-3/DTS/LPCM |
| AVCHD | `Avchd5` (4300 MB), `Avchd9` (7950 MB) – BDMV on DVD media | `BDMV/` in AVCHD form | H.264 within AVCHD limits |

"MB" in the planner is MiB (`1 MiB = 8192 kbit`, `calculator::bytes_to_kbit`: `1 kbit = 128 bytes`).

**Reference hardware.** Sony PS4 (FW 10.50) for Blu-ray navigation; Sony PS3 and PS4 for AVCHD on
DVD-RW (both verified, 1.5.1); standalone DVD players for DVD-Video. VLC/libbluray accepts many
HDMV and playlist errors that real players reject – **VLC is never proof of compliance**.

**Principles.**
- **Native, deterministic authoring.** The only external programs are `ffmpeg`/`ffprobe` (`bin/`,
  then the working directory, then `PATH`). Menu renderer, subpicture muxer, DVD authoring (VM, VOB,
  IFO), BDAV muxer, UDF/ISO writers and burn engines are Rust in this repo. Every disc address is
  planned before anything is written, so output is reproducible and testable.
- **Ported where bytes must match.** `dvdvm.rs`, `dvd_mux.rs`, `ifo.rs`, `spumux.rs` follow
  dvdauthor/spumux (GPL-2.0); `bd_mux.rs`, `bd_clip.rs` and the AVCHD index extension follow
  tsMuxeR 2.7.0 (Apache-2.0). Output is checked byte for byte against recorded original-tool output
  (`tests/dvdauthor_native.rs`, `tests/spumux_native.rs`, `tests/tsmuxer_native.rs`). Project
  license: GPL-3.0-or-later (`THIRD_PARTY_NOTICES.md`).
- **Clean room w.r.t. ConvertXToDVD.** Only its template `.ini` files are read at runtime (from the
  user's installation); none of its code.
- **Deterministic sizing.** The size plan is built from measured constants (section 3.3), and the
  finished build must stay at or under the disc target.
- **Hardware-verified compliance.** A disc-format change is only "done" when a real player accepted
  it; record the result in `handbook/testing.md` ("Hardware results").
- **Honest UI.** Unsupported options stay visible but disabled with the reason; long steps show
  progress and ETA; cancel kills every child process.

---

## 2. Repository architecture & module map

The request-style split "`src-tauri/` = Rust crate, `src/` = frontend" is **wrong for this repo**:

```text
Cargo.toml            workspace: members ".", "src-tauri"; default-members "." (engine + CLI)
src/                  engine library `musadvd` + CLI binary `musadvd`
src/burner/           disc images, drive access, burning, Burn Studio logic
src-tauri/            Tauri 2 desktop app `musadvd-gui` (thin command layer over the engine)
frontend/             vanilla TypeScript (no React), built by Vite into dist/
tests/                integration tests + tests/fixtures/{dvdauthor,spumux,tsmuxer,textsub}
assets/               icons (SVG), menu music (assets/menu-audio/*.opus), fonts
bin/                  ffmpeg.exe / ffprobe.exe (git-ignored, ~220 MB each, copied in by hand)
packaging/aur/        PKGBUILD
scripts/              check-i18n.mjs, check-order.mjs, build_all.ps1, update_shortcuts.ps1
tools/oracle/         rebuilds dvdauthor.exe / spumux.exe for fixture recording (not shipped)
handbook/, docs/      reference docs; docs/plans = history, not reference
.github/workflows/    ci.yml (checks), release.yml (nightly + tagged releases)
.claude/check.sh      fast checks for the status report (npm test + cargo unit tests)
```

### 2.1 Engine (`src/`)

| Module | Responsibility |
|---|---|
| `lib.rs`, `main.rs`, `cli.rs`, `studio_cli.rs` | Module list; CLI (clap) shared by the `musadvd` binary and the GUI exe's in-process CLI mode; Burn Studio sub-commands. |
| `dvd.rs` | `build_dvd` (entry for every disc type; hands Blu-ray/AVCHD to `bd::build_bd`). Owns `BuildOptions`, `StreamSelection`, `BuildError`, merge groups (`groups`, `check_title_count`), mixed subtitle order (`subtitle_order`), the size plan (`bitrate_plan`, `bitrate_plan_at`, `min_video_kbps`, `hw_reserve_mb`, `weighted_video_kbps`), work dir (`work_dir`, `keep_work`, `remove_dir_retry`, `WORK_DIR = ".musadvd-work"`). |
| `bd.rs` | `build_bd`: per-title H.264 encode (`title_encoders`, `HW_MAX_JOBS = 2`), one mux run per title, menus, `write_navigation(bdmv, nav, avchd)`, `avchd_versions`, frame rates (`BD_RATES`, `bd_frame_rate`). |
| `calculator.rs` | `DiscType`, `DiscLimits` (`limits()`), `calculate_capped` / `calculate_floored`, `EncodingProfile` caps, `Quality`. |
| `config.rs` | `AppSettings` (JSON, `#[serde(default)]`), `sanitized()`, `apply_to(&mut BuildOptions)`, `title_resolution(disc)`, `HwAccelMode::bd_encoder()`, Burn Studio settings tree. |
| `probe.rs` | ffprobe JSON → `MediaInfo`; track labels; `EXTERNAL_SUB_SOURCE_BASE = 10_000`; scene chapters (`DEFAULT_SCENE_THRESHOLD = 0.35`, `MIN_SCENE_GAP_SECS = 15`); trim/crop limits (`MIN_TRIM_SECS = 1`, `MIN_CROP_SIZE = 64`). |
| `encoder.rs` | ffmpeg argument building: MPEG-2/AC-3 program stream (`EncodeJob`, `run_encode_with_external`), BD H.264 (`BdEncodeJob`, `QSV_BD_OPTIONS`, `AMF_BD_OPTIONS`), audio planning (`plan_audio_kbps`, `plan_audio_with`, `ac3_settings`, `snap_ac3_kbps`, `AC3_KBPS_DVD`, `AC3_KBPS_BD`), copy rules (`bd_video_copy`, `bd_audio_copy`, `dvd_video_copy`), `source_chain`, `preview_filter`, quality test. |
| `tools.rs` | The **only** place that spawns processes (`tool_command`, `Toolchain::command`, `blocking_tool_command`); hardware encoder/decoder probes. |
| `author.rs` | DVD structure (`author_dvd`), menu PGCs (`menu_pre`, `compute_navigation`), limits (`MAX_TITLES`, `MAX_CHAPTERS`, `MAX_BUTTONS`, `MAX_WIDE_MENU_BUTTONS`, `MAX_PGC_COMMANDS`). |
| `dvdvm.rs`, `dvd_mux.rs`, `ifo.rs`, `spumux.rs` | dvdauthor/spumux ports: VM compiler, VOB/NAV fixing, IFO tables, subpicture muxer. |
| `template.rs`, `menu.rs`, `layout.rs`, `menu_anim.rs`, `icons.rs`, `bd_template.rs` | ConvertX template parsing/rendering, `CustomLayout` model + validation + DVD renderer, intro animation, SVG icons, templates on Blu-ray. |
| `bd_menu.rs`, `igs.rs`, `hdmv.rs` | HDMV navigation plan, IG segments (`ods_segments`), HDMV commands, `index.bdmv` / `MovieObject.bdmv`, `avchd_index`, `AVCHD_INDEX_EXTENSION`. |
| `bd_author.rs`, `bd_mux.rs`, `bd_clip.rs`, `h264.rs` | Mux jobs per title; native BDAV muxer (`mux_title`, `validate_sup`, `scan_video`, `TimingRepair`); CLPI/MPLS writers; H.264 SPS/SEI/buffer model. |
| `textsub.rs`, `subpic.rs`, `pgs.rs`, `fonts.rs`, `chapters.rs` | Text subtitle rendering (`SubtitleStyle`), bitmap subtitles to DVD subpictures, PGS writer, system fonts + `cjk_region`, chapter-file import. |
| `eta.rs`, `i18n.rs` | Remaining-time estimates over the five phases; engine translations (`tr`, `tf!`). |

### 2.2 Burner (`src/burner/`)

| Module | Responsibility |
|---|---|
| `mod.rs` | `burn_video_ts` (the single burn path of GUI and CLI), `create_disc_image`, `OpticalBurner` trait, `system_burner()`, `MediaInfo`, `MediaType`, `DiscContent::of_folder`, `media_problem`, Auto speed (`auto_speed`, `preferred_speed_for`, `auto_speed_text`), `is_bdmv_folder`, `bd_image_root`, `DVD5_SECTORS = 2_295_104`, `DVD9_SECTORS = 2_295_104 + 2_086_912`. |
| `udf.rs` | UDF 1.02 + ISO 9660 bridge (DVD-Video `plan_image`, file trees `plan_tree`, `write_image`). |
| `udf250.rs` | UDF 2.50 with metadata partition (`plan_bd_image`, `create_bd_image`), `BD25_SECTORS = 12_219_392`, `BD50_SECTORS = 24_438_784`. |
| `layerbreak.rs`, `label.rs`, `sidecar.rs`, `isoread.rs`, `css.rs`, `tools.rs` | DVD-9 layer break; volume labels; `.dvd/.cue/.mds`; ISO reader; CSS detection (no decryption); offline label/layer-break tools. |
| `source.rs`, `builder.rs`, `reader.rs`, `verifier.rs`, `ringbuf.rs`, `graph.rs`, `studio.rs` | Burn Studio: sector sources, image build (content detection `detect`), disc read, verify, ring buffer, telemetry/`.ibg`, operations. |
| `spti.rs`, `mmcwrite.rs`, `windows.rs`, `unix.rs`, `power.rs` | MMC command layer + `ScsiTransport`; direct write engine and `blank_disc`; IMAPI2 burner; Linux SG_IO burner; keep-awake, tray, `open_raw_volume`. |

### 2.3 Desktop app (`src-tauri/src/`)

| File | Responsibility |
|---|---|
| `main.rs` | Commands: `probe_file`, `list_templates`, `template_preview`, `calculate_project` (→ `project_calculation_with`, `h264_quality`, `full_d1_equivalent_kbps`), `source_frame`, `detect_crop`, `test_title_quality`, `detect_scene_chapters`, `start_conversion` (events `convert-event`, `convert-done`), settings, locales. |
| `burn.rs` | `list_drives`, `probe_media`, `start_burn`, `cancel_burn`, `startup_burn`, `open_folder`; events `burn-progress`, `burn-done`. |
| `burn_studio.rs`, `menu_editor.rs`, `preview.rs`, `self_update.rs` | Burn Studio commands; menu-editor commands (`check_layout`, `menu_navigation`, …); loopback fMP4 live player; AppImage/portable self-update. |

`tauri.conf.json` + `capabilities/` define windows and permissions. Without the
`tauri/custom-protocol` feature the exe loads `http://localhost:5173` instead of the embedded `dist/`.

### 2.4 Frontend (`frontend/src/`, vanilla TypeScript)

Windows: `index.html` → `main.ts`, `editor.html` → `editorWindow.ts`, `burnStudio.html` →
`burnStudio.ts`, all through `boot.ts`. Key modules: `api.ts` (typed command wrappers, mirrors Rust
types), `main.ts` (project, tabs, size estimate, conversion), `settings.ts` (`formKit`,
`openSettings`, `renderProjectSettings`, `PROJECT_SETTING_KEYS`), `project.ts` (`.musadvd` files,
`withProjectSettings`), `order.ts` (pure list helpers, also run by `scripts/check-order.mjs`),
`quality.ts`, `discRules.ts`, `burnConfirm.ts`, `burn.ts`, `edit.ts`, `menuEditor.ts`, `layout.ts`,
`studio/*`. Full table: `handbook/modules-gui.md`.

### 2.5 Data flow

```text
GUI state (main.ts: titles, tracks, merges, PROJECT_SETTING_KEYS settings)
  -> calculate_project (live estimate)          -> dvd::bitrate_plan_at  (same plan as the build)
  -> start_conversion -> BuildOptions (config::apply_to + StreamSelection per input)
  -> dvd::build_dvd
       DVD:  probe -> select_streams/subtitle_order -> [1] menus (template.rs/menu.rs or layout.rs -> spumux)
             -> bitrate_plan_at(menu_mb measured) -> [2] encoder.rs: ffmpeg MPEG-2+AC-3 piped through
             native spumux stages -> [3] author_dvd -> dvdvm/dvd_mux/ifo -> VIDEO_TS
       BD/AVCHD (bd::build_bd): probe -> menus (bd_menu/igs, measured size) -> bitrate_plan_at
             (+ hw_reserve_mb) -> per-title H.264 (x264 or nvenc/qsv/amf) -> bd_mux::mux_title + bd_clip
             -> write_navigation (+ avchd_versions for AVCHD) -> BDMV
  -> burn dialog -> burner::burn_video_ts
       DVD: layerbreak::plan_disc -> create_disc_image (UDF 1.02 + ISO bridge)
       BD/AVCHD: udf250::plan_bd_image -> create_bd_image (UDF 2.50)
  -> OpticalBurner::burn_iso (IMAPI2 / direct MMC / SG_IO) -> optional read-back verify
```

Phases shown to the user (`eta::JobPhase`): menus, videos, authoring, ISO, burn.

---

## 3. Critical domain & hardware invariants (DO NOT BREAK)

### 3.1 AVCHD vs Blu-ray disc layout

| Item | BD-25 / BD-50 | AVCHD-5 / AVCHD-9 (DVD media) |
|---|---|---|
| Folder | `BDMV/` at the root, long names | `BDMV/` at the root, long names (**never** `AVCHD/BDMV` + 8.3 names – that is the FAT camcorder-card layout) |
| Header version, bytes 4..8 of `index.bdmv` (`INDX`), `MovieObject.bdmv` (`MOBJ`), `PLAYLIST/*.mpls` (`MPLS`), `CLIPINF/*.clpi` (`HDMV`), BACKUP copies included | `0200` | `0100` |
| `index.bdmv` ExtensionData | none (bytes 12..16 = 0) | `hdmv::AVCHD_INDEX_EXTENSION` (406 bytes, ID1 `0x1000` / ID2 `0x0100`, `IDEX` at block+0x18), appended 4-byte aligned, address in bytes 12..16 (`hdmv::avchd_index`; one title → 0x78, as tsMuxeR) |
| `AUXDATA/` | present | present |
| `BDJO/`, `JAR/`, `META/`, `BACKUP/BDJO`, `BACKUP/JAR` | present | removed when empty (`bd::avchd_versions`) |
| `CERTIFICATE/BACKUP/` | present (created in `build_bd`) | absent |
| Image | UDF 2.50 (`udf250.rs`) | UDF 2.50 (`udf250.rs`) |

Only the version digits differ in MPLS/CLPI/MOBJ – their structure is the BD one, exactly as tsMuxeR
writes AVCHD (`blurayHelper.cpp`: `getBDMV_VersionNumber`, `bdIndexData` from offset 0x78).

**Why players reject the wrong form (observed, not firmware-documented):** a disc on DVD media that
identifies as Blu-ray (`0200`, no IDEX) was shown as "Data Disc" by the PS3 and refused by the PS4
(klog: `PromoterBDVDGetNumOfFormats() returned 0x3`, `unsupproted disc_type(0x13)` for a DVD-RW).
With `0100` + IDEX + AUXDATA both mounted and played it. The `AVCHD/BDMV` 8.3 layout was tried and
reverted (commit `15b430a`). Tests: `tests/bluray.rs::avchd5_builds_a_dvd_sized_bdmv` (every version,
IDEX position, folders, size, libbluray read of a 200 s build) and
`two_clips_make_two_playlists_with_their_own_rates` (BD-25 keeps `0200`, `BDJO`, `CERTIFICATE`).

**Blu-ray limits (`DiscLimits`):** BD: video 4000–35,000 kbit/s, video+audio ≤ 48,000, VBV
maxrate 40,000 / buffer 30,000 kbit. AVCHD: video 2000–15,000, total ≤ 18,000, VBV 18,000/18,000.
Profiles cap video: BD 35,000 / 20,000 / 12,000 (Auto+SP / MP / LP), AVCHD 15,000 / 10,000 / 6000.
Frame rates (`bd::BD_RATES`): 23.976, 24, 25i, 29.97i (fake-interlaced 1080, always x264),
50, 59.94 (720p). **No 1080p25/29.97 on Blu-ray.** Film sources keep 23.976/24 (`bd_frame_rate`).

### 3.2 UDF & ISO filesystem rules

- **DVD-Video: UDF 1.02 + ISO 9660 bridge** (`burner/udf.rs`, 2048-byte sectors): system area
  0..15, ISO PVD 16, terminator 17, UDF VRS 18..20 (`BEA01`, `NSR02`, `TEA01`), main VDS 32..47,
  reserve VDS 48..63, LVID 64..65, ISO path tables 66..67, ISO dirs from 68, anchor 256, partition
  from 257 with file data **in DVD-Video order** (`VIDEO_TS.IFO/VOB/BUP`, then per titleset
  `VTS_nn_0.IFO`, `VTS_nn_0.VOB`, `VTS_nn_1..9.VOB`, `VTS_nn_0.BUP`), contiguous, last sector =
  closing anchor. UDF revision `0x0102`.
- **Blu-ray/AVCHD: UDF 2.50 only, no ISO bridge** (`burner/udf250.rs`): VRS `BEA01`, `NSR03`,
  `TEA01` (16..18), main VDS 32, reserve VDS 48, LVID 64, anchor 256, physical partition from
  sector 512: Metadata File EFE (file type 250) at P+0, Metadata Mirror EFE (type 251) at P+1,
  metadata copy 1 at P+32, file data with **every file on a 32-block boundary** (`UNIT = 32`),
  metadata mirror (duplicate flag), closing anchor. Partition map `*UDF Metadata Partition`,
  revision `0x0250`, descriptor version 3. Files above 4 GiB are why there is no ISO bridge.
- **DVD-9 layer break** (`layerbreak.rs`): L0 ≥ L1, L0 a multiple of 16 sectors, break on a
  title-cell start; otherwise zero padding in front of a titleset and a patched `TT_SRPT`.
  `TT_SRPT` start sectors are relative to `VIDEO_TS.IFO`.
- **Volume labels** (`label.rs`): ISO label `sanitize_volume_label` – A–Z, 0–9, `_`, max 32
  (`MAX_LABEL_LEN`), default `DVD_VIDEO`; UDF label free text Latin-1, max 30 (`MAX_UDF_LABEL_LEN`,
  one 32-byte CS0 dstring); empty UDF label = the ISO label. Frontend mirror: `frontend/src/label.ts`.
- **Windows read-back boundary:** reads on a volume handle (`\\.\X:`) stop at the file system's end,
  one sector before the image's closing UDF anchor, so verify failed at the tail.
  `burner::power::open_raw_volume` enables `FSCTL_ALLOW_EXTENDED_DASD_IO`. Keep it on every raw
  read path (verify, Burn Studio read/verify).
- Capacities: DVD-5 2,295,104 sectors, DVD-9 + 2,086,912, BD-25 12,219,392, BD-50 24,438,784.

### 3.3 Bitrate & container overhead math (`dvd::bitrate_plan_at`)

Base video bitrate for the whole disc (weights scale it per title; `QUALITY_LOW = 0.75`,
`QUALITY_HIGH = 1.25`):

```text
fixed  = disc_share + menu_mb*8192 + subs_kbit + copied_kbit (+ hw reserve, passed in as menu MB)
target = disc.target_mb * 8192
fixed + (1 + ratio) * Σ (video_kbps*weight + audio_kbps) * duration_s  <=  target
video  = floor((target - fixed - (1+ratio)*audio_kbit) / ((1+ratio) * weighted_secs)),
         clamped to [min_video, min(profile cap, max_total - audio)]; weighted titles re-solved by
         binary search against the real per-title sizes (weighted_video_kbps pins floors/ceilings)
```

| Term | DVD-Video | Blu-ray / AVCHD |
|---|---|---|
| `disc_share` | `DVD_NAV_KBPS (40) * total_secs + DVD_IFO_KBIT_PER_TITLE (64*8.192) * titles` | 0 |
| `ratio` (stream overhead) | `DVD_PACK_OVERHEAD_RATIO = 0.027` | `BD_STREAM_OVERHEAD_RATIO = 0.08` (measured 7.42 % over the encoded files, 7.7 % of raw streams) |
| subtitles | `SUBTITLE_KBPS = 10` per track | `BD_SUBTITLE_KBPS = 40` per track (rendered 1080p PGS measured ~32), × (1 + ratio) |
| copied titles | exact bytes (`copy_bytes`) or `(copy_video + audio) * secs` | the same × (1 + ratio) |
| menus | measured size of the built menus | measured size (`MenuStage.size_mb`); GUI estimate: 1.1 MB per menu-clip second |
| hardware encoder | – (DVD always CPU) | `hw_reserve_mb = target_mb * HW_ENCODER_RESERVE (0.005)` when NVENC/QSV/AMF is selected – build and `calculate_project` alike |

**Bitrate floors** (`dvd::min_video_kbps`): DVD Full D1 1000 (`MIN_VIDEO_KBPS`), Half D1 600,
SIF 400; Blu-ray 1080p 2500, 720p 1500; AVCHD keeps its 2000 where lower. With mixed Blu-ray frame
sizes the lowest floor applies to the whole plan (`bd.rs`). `Clamp::Min` = does not fit →
`BuildError::DoesNotFit` before encoding.

**DVD limits:** video ≤ `MAX_VIDEO_KBPS = 8500` (plan), peak `MAX_VIDEO_PEAK_KBPS = 9800`,
video+audio ≤ `DVD_MAX_TOTAL_KBPS = 9800`, mux rate `DVD_MUX_RATE_KBPS = 10080`, VBV
`VBV_BUFFER_KBIT = 1835`. LP profile switches Full D1 to Half D1 below `LP_HALF_D1_BELOW_KBPS = 3000`
(`effective_resolution`).

**Audio:** encoded AC-3 = `audio_bitrate_kbps` snapped to `AC3_KBPS_DVD = [96,128,160,192,224,256,
384,448]` or `AC3_KBPS_BD = [192,224,256,320,384,448,512,576,640]` (nearest, tie → lower); auto =
448 for ≥ 5 channels else 192. LPCM is sized at its wire rate `rate_khz * bits * (ch + ch % 2)`.

**Quality rating (GUI):** DVD as the Full D1 bitrate with the same bits per pixel
(`full_d1_equivalent_kbps`), linear between the disc's min/max; Blu-ray/AVCHD by bits per pixel at
23.976 fps (`h264_quality`): points (0.03→0, 0.05→40, 0.09→65, 0.16→85, 0.30→100 %) linear in
log(bpp); bands 40 fair / 65 good / 85 excellent.

**Never** change a constant here without a measurement build (procedure in
`handbook/testing.md` → "Measuring the size model"). Regression tests:
`dvd::tests::avchd_plan_leaves_room_for_the_measured_bdav_overhead`,
`sif_floor_fits_a_19_hour_collection_on_dvd9`, `bd25_holds_a_19_hour_720p_collection`,
`low_title_hitting_the_floor_still_fits_{dvd5,bd25}`.

### 3.4 Disc standard limits

- **99 titles** (`author::MAX_TITLES`): merged inputs (`StreamSelection::merge_with_previous`) are one
  title; `dvd::check_title_count` counts `groups(...)`, used by DVD and Blu-ray. The GUI counts
  `titleGroups()` plus menu pages (`getMenuAndVideoCounts`).
- **99 chapters per title** (`author::MAX_CHAPTERS`): a merged DVD title sums its members'
  chapters; the engine fails (`"… chapters after merging, a DVD allows 99 per title"`), the GUI
  blocks Convert first and offers "Split automatically" (`order.ts` `chapterOverflows`,
  `autoSplitPoints`: greedy, as late as possible). Single files are truncated to 99.
- **Tracks:** DVD ≤ 8 audio (`MAX_AUDIO_STREAMS`), ≤ 32 subtitle (`MAX_SUBTITLE_STREAMS`).
  The list order is the disc order; embedded and external subtitles form one list
  (`StreamSelection::subtitle_order`, external = `EXTERNAL_SUB_SOURCE_BASE + i`).
- **DVD resolutions** (`author::Resolution`): Full D1 720×576/480, Half D1 352×576/480, SIF
  352×288/240; Half D1/SIF are 4:3 only (16:9 sources letterboxed). Menus stay Full D1.
- **Menus:** ≤ 36 buttons (`MAX_BUTTONS`), ≤ 18 on 16:9 menus (`MAX_WIDE_MENU_BUTTONS`, anamorphic
  frame with widescreen stream 0 + letterbox stream 1), ≤ 128 VM commands per PGC
  (`MAX_PGC_COMMANDS`); Blu-ray ≤ 255 buttons (`layout::MAX_BD_BUTTONS`).
- **Highlight initialization (`SetHL_BTN`):** every DVD menu PGC with buttons starts with
  `s8 = 1k;` (SetHL_BTN 1 = `56 00 00 00 04 00 00 00`) unless its own commands set `s8`/`button`
  (`author::menu_pre`). Coming from a page with more buttons would otherwise leave SPRM 8 past the
  last button (strict players show no highlight); dispatcher PGCs (command-only) get no reset.
  Auto-action buttons are never the initial focus (`layout.rs` emits `s8 = Nk;` for the first
  button without auto-action). Blu-ray: every menu runs `SetButtonPage(1)` before its loop; the ICS
  has no default button and the first non-auto button gets id 1 (`bd_menu.rs`).

### 3.5 HDMV navigation (PS4 rules, `bd_menu::plan`)

First Play and Top Menu are interactive titles; every disc title is a movie title. A movie title
**never** uses `JumpObject`; it ends with `JumpTitle 0` after storing the return menu object in
`GPR8` (Top Menu dispatches and clears it). Still menus: MPLS `still_mode = 2`, `still_time 0`,
loop `SetButtonPage(1); PlayPL; Goto 1`. Settings-menu stream buttons only store choices
(`GPR16 + 2*(t-1)` audio, `+1` PG or `0xFFFF`); the title object applies them before `PlayPL`
with register-form `SetStream` (flags immediate, number fields name GPRs). Register map: `GPR1`
chapter mark, `GPR2` play-all, `GPR3`-`GPR5`/`GPR7` intro dispatcher, `GPR6` scratch, `GPR8` return
menu, `GPR16+` per-title streams, `GPR4000/4001` global audio/subtitle. DVD globals: `g11` audio,
`g12` SPRM2 value (`layout::GLOBAL_AUDIO/GLOBAL_SUBTITLE`); `g13`–`g15` are VM temporaries.

---

## 4. Encoding, muxing & menu authoring pipeline

- **Probe** (`probe::probe`): ffprobe JSON → `MediaInfo` (streams, chapters, HDR, trim/crop
  applied later by `dvd::apply_selection`). External subtitles: `probe_external_subtitle`.
- **Stream selection** (`dvd::select_streams`, `select_external_subtitles`): explicit GUI order
  wins over language filters; unsupported bitmap codecs and RTL text on DVD produce notes.
- **DVD encode** (`encoder::run_encode_with_external`): one ffmpeg run → MPEG-2 + AC-3 program
  stream on `pipe:1`, through one native spumux stage per subtitle stream
  (`Toolchain::ffmpeg_spumux_pipeline`); subtitle `n` becomes disc stream `stream_ids[n]` (mixed
  order). Two-pass: fast statistics pass 1 (same GOP/rate control), pass 2 `-mbd rd` plus
  `-trellis 1 -intra_vlc 1` unless fast. Audio sync filter `aresample=48000:async=1:min_hard_comp=0.01`.
- **Copy paths:** `dvd_video_copy` (compliant MPEG-2, untrimmed/uncropped/unfiltered),
  `bd_video_copy` (+ build-time `bd_mux::scan_video` buffer/IDR check, `TimingRepair` adds BD
  timing), `bd_audio_copy` (AC-3 ≤ 640, DTS core, integer PCM/FLAC ≤ 24 bit as LPCM; AVCHD:
  no DTS, LPCM only 48 kHz/16 bit/≤ 2 ch). Trimmed or loudnormed titles never copy audio.
- **GPU decode/encode:** `hw_decode` default on; `probe_hwaccel` tries vendor decoders
  (`d3d11va`/`cuda`/`qsv` on Windows, `cuda`/`vaapi`/`qsv` on Linux) with a 2 s probe. The probe
  ignores demuxer warnings such as MP4 "UDTA parsing failed" (`tools.rs`, "True when a hwaccel
  probe log reports a failure"); only decoder failures fall back to the CPU. Hardware H.264 per
  title via `title_encoders` (1080i always x264); unavailable encoders warn and fall back.
- **BDAV mux** (`bd_mux.rs`): ffmpeg `-c copy` → MPEG-TS pipe → re-packetised 192-byte m2ts with
  PCR/PAT/PMT/SIT (`BASE_PTS = 54_000_000`, `PCR_DELTA = 3000`, `PSI_DELTA = 5000`,
  `SIT_INTERVAL = 76_900`, 32-packet alignment), ATS interpolation; `bd_clip.rs` writes CLPI/MPLS.
  A clip without an IDR+SPS (empty EP map) is rejected; every playlist keeps ≥ 1 entry mark.
- **PGS text subtitles** (`pgs.rs`): one epoch per cue (overlapping cues replace each other);
  objects over 64 KB are split into ODS fragments (`igs::ods_segments`: first fragment flag 0x80
  with a 24-bit object length, last 0x40; every segment payload ≤ 0xFFFF). `bd_mux::validate_sup`
  checks every `.sup` before muxing ("invalid .sup format at byte N", names the track).
- **CJK fonts** (`fonts::cjk_region(lang, title)`): Japanese, Hong Kong, Traditional, Simplified
  Chinese, Korean glyph forms from the track language and title hints (繁體/TC, 简体/SC, HK);
  `preferred_families` orders fonts; `.ttc` face index from fontdb (`SystemFont.index`).
- **Menus:** ConvertX templates (`template.rs` → `menu.rs` background + two 4-colour masks →
  spumux) or `CustomLayout` JSON from the menu editor (`layout.rs` / `frontend/src/layout.ts`,
  `LAYOUT_VERSION = 1`; `validate` / `validate_bd`). Buttons compile to DVD VM commands
  (`dvdvm.rs`) or HDMV commands (`hdmv.rs`) via `bd_menu.rs` and IGS (`igs.rs`). Intros/motion
  menus: `menu_anim.rs`, frames piped to the encoder.
- **Navigation & Test Remote:** remote links come from `author::compute_navigation`, then
  `layout::apply_nav_overrides` (manual D-pad targets); `layout::menu_button_navigation` returns
  the same links to the editor (`menu_editor.rs` `menu_navigation`), so Test Remote
  (`menuEditor.ts` `loadRemoteNav`, `flashRemoteActivate`, `openRemotePreview`; play-mode
  countdowns for `once`/`onceActivate`) follows exactly what the disc does. `layout.ts` has a
  fallback navigation if the backend is unavailable.

---

## 5. Optical burning & erasing

- **Entry point:** `burner::burn_video_ts(job, burner, cancel, progress)` – image (DVD:
  `layerbreak::plan_disc` + `create_disc_image`; BD/AVCHD: `udf250::plan_bd_image` +
  `create_bd_image`), `OpticalBurner::burn_iso`, optional verify, image removed unless `keep_iso`.
  GUI (`src-tauri/src/burn.rs`) and CLI (`musadvd burn`) both use it; tests use a fake
  `OpticalBurner` (`tests/burn_flow.rs`).
- **Backends:** Windows `windows.rs` (IMAPI2: `IDiscMaster2`, `IDiscRecorder2`,
  `IDiscFormat2Data`, default) or `mmcwrite.rs` direct MMC over SPTI (`io.backend = direct`); Linux
  `unix.rs` native MMC over SG_IO (`SgIoTransport`, `/dev/srN` opened `O_NONBLOCK`, needs the
  `cdrom`/`optical` group), growisofs/xorriso only as fallback. All MMC goes through
  `spti::ScsiTransport`. macOS unsupported.
- **Media detection:** `probe_media` → `MediaInfo { media_type, physical, blank, rewritable,
  writable, free_sectors, total_sectors, speeds, layer_break_supported }`. Direct-write profiles
  (`mmcwrite` `Kind::of`): DVD+R/DL `0x1B/0x2B`, DVD+RW `0x1A/0x2A`, DVD-R/RW sequential/DL
  `0x11/0x14/0x15/0x16`, DVD-RW restricted overwrite `0x13`, BD-R `0x41/0x42`, BD-RE `0x43`.
  BDMV content may go on DVD media only as AVCHD (`DiscContent::fits_dvd`, `media_problem`).
- **Erase:** `mmcwrite::blank_disc` sends MMC BLANK (0xA1, IMMED; minimal 0x01 / full 0x00)
  exactly once (cannot be cancelled), polls TEST UNIT READY and reads sense-key-specific progress
  (2/04/07); GET CONFIGURATION guards CD-RW/DVD-RW only (DVD+RW/BD-RE need no erase, write-once is
  refused). Used by Burn Studio Erase, `musadvd erase`, direct/Linux burns with `erase`; IMAPI keeps
  IMAPI erase. GUI: erase pre-ticked for rewritable discs with data (`burnConfirm.eraseDefault`),
  confirmation before erasing or writing write-once media (`confirmKind`).
- **Speed:** Auto from the speeds the drive reports for the inserted disc (`auto_speed`):
  `auto_speed_x` ceiling, BD-RE 2x, rewritable DVD its fastest reported speed, BD-R 4x.
- **Verify:** `verifier.rs` compares disc and image (raw volume read with extended DASD I/O, see
  3.2) and maps miscompares to files; the reader zero-fills unreadable sectors.
- **Safety:** never burn, erase or write to a drive without the user's explicit go. The automated
  suite never touches a real drive.

---

## 6. Development, testing & release workflows

**Shell (Windows, every session):** the user PATH contains a stray quote; prefix PowerShell with
`$clean = ($env:Path -split ';' | % { $_.Trim().Trim('"') } | ? { $_ }) -join ';'; $env:Path = "$env:USERPROFILE\.cargo\bin;$env:USERPROFILE\scoop\apps\mingw\current\bin;$clean"`.
Toolchain: rustup host `x86_64-pc-windows-gnu` (MinGW `dlltool`), extra target
`x86_64-unknown-linux-gnu` for the Unix compile check; Node 24 / npm 11.

```powershell
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
cargo test --workspace            # all 33 binaries, ~3+ min, needs bin/ffmpeg + ConvertX templates
cargo test --workspace --lib --bins                      # unit tests only (~2 min cold)
cargo check --lib --tests --target x86_64-unknown-linux-gnu
npm run build                     # check-i18n + check-order + tsc --noEmit + vite build
npm test                          # vitest (pure TS modules, *.test.ts)
cargo build --release -p musadvd-gui --features tauri/custom-protocol   # GUI exe with embedded dist/
cargo build --release             # CLI
```

- Close `musadvd-gui` before building it. Integration tests skip gracefully without tools/templates.
- Fixtures from original tools: `SPUMUX_RECORD`, `DVDAUTHOR_RECORD`, `TSMUXER_RECORD=<exe> cargo test
  --test <name>`; `UPDATE_GOLDEN=1` for textsub goldens.
- ffmpeg's `bluray:` protocol ignores playlists under 3 minutes – libbluray checks need long clips.
- `tests/process_hygiene.rs` fails on any `Command::new` in `src/`/`src-tauri/src/` outside `tools`.

**CI** (`.github/workflows/`):
- `ci.yml` (Windows + Ubuntu, every push/PR): `npm ci`, `npm run build`, `npm test`,
  `cargo check --workspace --tests`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --lib` (no ffmpeg on runners; tool-dependent tests skip).
- `release.yml`: job `ci` (reuses `ci.yml`) → `windows` (`needs: ci`: fetch ffmpeg full build,
  CLI build, clear old nightly assets, `tauri-action` NSIS installer, portable ZIP) → `linux`
  (`needs: windows`: deb, AppImage, portable tar.gz). Releases are gated behind green CI.
  Push to main → rolling `nightly` pre-release (assets cleared first, so it holds only the current
  build); tag `v*` → versioned release + `latest.json` for the updater. The updater reads only
  `releases/latest/download/latest.json` (non-prerelease).

**Releasing (only when the user asks):**
1. Full checks: `cargo test --workspace`, clippy, Linux check, `npm run build`, `npm test`.
2. Bump the version in all of: `Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`
   (the tag must match it), `package.json`, `package-lock.json` (two places), `Cargo.lock`
   (`musadvd`, `musadvd-gui`), `packaging/aur/PKGBUILD`, the `STATUS.md` header line.
3. `CHANGELOG.md`: new `## [x.y.z] - YYYY-MM-DD` under `## [Unreleased]` (Keep a Changelog:
   Added / Changed / Fixed, user-facing wording).
4. Commit `Release x.y.z: <summary>`, `git tag -a vx.y.z -m "MusaDVD vx.y.z"`, push main and the tag.
   Features → minor, fixes only → patch.

---

## 7. Agent working protocol & contribution rules

**Before touching authoring, muxing, sizing or burning code:**
1. Read `STATUS.md` section 5 (dead ends) and the relevant `handbook/` page.
2. Grep every caller of the function you change; fix root causes once in the shared code.
3. For disc formats: compare with the reference implementation (dvdauthor, spumux, tsMuxeR source)
   – never guess byte layouts. Keep the byte-level fixture tests green; re-record fixtures only with
   the original tool.
4. For sizing: change constants only from a measurement build (`--keep-work`, compare m2ts/VOB with
   encoded files and ffprobe packet sizes) and add a regression test with the measured numbers.
5. For burning: use the fake `OpticalBurner` / scripted transport; no real drive without the user's go.

**Rules:**
- Every file is English (code, comments, Markdown, commits, engine errors). GUI text comes from
  `frontend/src/locales/*.json` (`en.json` master; `de`, `ru` translations; flat keys,
  1-space-indented JSON, `{placeholders}` identical; no umlauts in `.ts` – `check-i18n.mjs`).
- Every external process goes through `tools::tool_command` / `Toolchain::command` /
  `blocking_tool_command`.
- Frontend logic goes into pure modules with a vitest next to them (`order.ts`, `quality.ts`,
  `discRules.ts`, `burnConfirm.ts`, `project.ts`); `main.ts` only wires them to the DOM. Frontend
  types that mirror Rust structs change together with the Rust side.
- Settings that describe the disc belong to `PROJECT_SETTING_KEYS` (saved per project); app-level
  options stay in the General dialog. Old settings/project files must keep loading (serde defaults,
  `withProjectSettings` type checks).
- Comments explain *why*; match the surrounding density. Patch scripts with backslashes go into a
  file, not a heredoc. Source files are LF.
- Disc-format or player-behaviour changes: record the player, firmware/disc and result in
  `handbook/testing.md` ("Hardware results") and a dead end in `STATUS.md` section 5 when a wrong
  hypothesis cost real time.
- Docs: keep `handbook/` and this file in sync with the code in the same change; `STATUS.md` is
  updated at milestones (`/status`), max 150 lines.
- Commit or push only when asked; micro-commit each green step of a larger change. Every commit
  message ends with the co-author line the session prescribes.
