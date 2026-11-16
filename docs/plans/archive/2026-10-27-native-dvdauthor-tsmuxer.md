# Plan: native DVD authoring + native BDAV muxer (remove dvdauthor.exe and tsMuxeR.exe)

Date: 2026-10-27 · Branch: new branch `native-authoring` from `native-text-subs` · Phases are strictly
sequential: Phase 1 is finished, green and committed before Phase 2 starts.

## Goal & approach

Only `ffmpeg`/`ffprobe` stay external. **Source-guided scoped port**: upstream C/C++ is the structural
reference, `bin/dvdauthor.exe` / `bin/tsMuxeR.exe` are golden oracles, whose outputs are **recorded as
fixtures** (like `tests/fixtures/spumux/`) so the oracle tests keep running after the binaries are deleted.
No Ghidra, no spec-only rewrite. GPL note: like `spumux.rs`, the new modules are GPL-derived (dvdauthor
GPL-2.0, tsMuxer Apache-2.0) – record in STATUS.md mission section.

References (git-ignored, `.ref-src/`, already cloned, deleted at the end):
- dvdauthor `fe8fe35` = exactly the commit `bin/dvdauthor.exe` was built from (tools/dvdauthor-win/README).
  Relevant: `dvdvob.c` (FindVobus, MarkChapters, FixVobus, remapbyte, transpose_ts), `dvdifo.c`,
  `dvdpgc.c`, `dvdauthor.c` (attributes, entries, jump resolution), **`dvdcompile.c` + `dvdvml.l` +
  `dvdvmy.y`** (the VM compiler – there is no `dvdvm.c`; `vm_optimize` peephole pass matters for
  byte identity), `da-internal.h`.
- tsMuxer `c6b1186` (HEAD). `bin/tsMuxeR.exe` is 2.7.0 – where HEAD and the binary differ, **the binary wins**.
  Relevant: `tsMuxer.cpp` (PAT/PMT/SIT cadence, PCR, ATS), `tsPacket.cpp` (`CLPIParser::compose*`,
  `MPLSParser::compose*`), `blurayHelper.cpp`, `pesPacket.cpp`, `h264StreamReader.cpp` (insertSEI/contSPS),
  `ac3StreamReader.cpp`, `dtsStreamReader.cpp`, `lpcmStreamReader.cpp`, `pgsStreamReader.cpp`.

Corrections to the task text (verified in the sources):
- SRI has **19** forward + 19 backward entries (timeline 1..15, 20, 60, 120, 240 half-seconds), not 240.
- Our meta uses `--no-pcr-on-video-pid`: PCR lives on **PID 0x1001** (`DEFAULT_PCR_PID`), video on **0x1011**.
- Our AVCHD output is BDMV-on-DVD built with `--blu-ray` → CLPI/MPLS version `0200` for both; keep
  that (no `0100`) unless the oracle shows otherwise.
- M2TS output can't be byte-identical to tsMuxeR (packet scheduling is heuristic). Target: byte identity
  for DVD (dvdauthor is deterministic), **field-level identity** for CLPI/MPLS minus SPN/ATC-derived
  values, structural identity + own validators for the M2TS.

## Phase 1 – native DVD authoring

### New/changed files
| File | Content |
|---|---|
| `src/dvdvm.rs` (new) | Parser for the command subset we emit (see grammar) → AST → 8-byte instructions; port of `dvdcompile.c` incl. `vm_optimize`, `if` combining, jump resolution per domain (VMGM/VTSM/title), `call … resume cell`. Unsupported syntax = `AuthorError` (no silent fallback). |
| `src/dvd_mux.rs` (new) | VOB scanning + fixup (FindVobus/MarkChapters/FixVobus port): pack scan, VOBU detection via ffmpeg's NAV packs, video/audio/SPU PTS per VOBU, consume + strip `dvdauthor-data` 0xBE sectors from `spumux.rs` (tag 1 CLUT, 2 group lanes/BUTTON_COLI, 3 buttons+nav), SPU colour remap (`remapbyte`), `transpose_ts` for merged VOBs, cells/chapters, then PCI (VOBU_S/E_PTM, C_ELTM BCD, HLI: HL_GI/BTN_COLIT/BTN_IT) + DSI (VOBU_EA, NV_PCK/C_IDN, VOB_IDN, SYNCI, SRI 19+19, next/prev VOBU, ILVU none). VOB splitting at 524,287 sectors (`VTS_nn_m.VOB`). |
| `src/ifo.rs` (new) | IFO/BUP writer (dvdifo.c + dvdpgc.c port): VMGI_MAT, TT_SRPT, VMGM_PGCI_UT, PTL_MAIT, VTS_ATRT, VMGM_C_ADT, VMGM_VOBU_ADMAP; VTSI_MAT, VTS_PTT_SRPT, VTS_PGCIT, VTSM_PGCI_UT, VTS_TMAPTI, VTSM_C_ADT, VTSM_VOBU_ADMAP, VTS_C_ADT, VTS_VOBU_ADMAP; PGC (commands, program map, cell playback/position, audio/SPU control, palette from CLUT). Addresses planned before writing (deterministic, matches STATUS principle). |
| `src/author.rs` | New `pub fn author_dvd(layout: &DvdLayout, work: &Path, dest: &Path) -> Result<AuthorLog, AuthorError>` = builder entry, takes `DvdLayout` directly (no XML round-trip). `dvdauthor_xml` stays (oracle tests + `render-menu` export, `menu.rs` `build_dvd.cmd`). `strip_intro_buttons` moves to `#[cfg(test)]`/test helper: native builder writes zero HLI in cell 1 of intro VOBs. Warnings that dvdauthor printed (e.g. PTS gaps) become `AuthorLog` lines. |
| `src/dvd.rs` | Step 8: call `author::author_dvd` instead of `tools.dvdauthor`; drop the post-pass `strip_intro_buttons`; keep writing `dvdauthor.xml` into the work dir? → no, write `authoring.log` instead (tests `menu_intro.rs:797`, `motion_menu.rs:266` read `dvdauthor.log` → rename there). |
| `src/tools.rs` | Remove `dvdauthor` field, `locate("dvdauthor")`, `Toolchain::dvdauthor`, hints; `engine.tools_dvd` loses `{dvdauthor}`. |
| `src/lib.rs` | `pub mod dvdvm; pub mod dvd_mux; pub mod ifo;` |

Command grammar actually emitted (grep of `src/`): `gN = <const>`, `audio = n`, `subtitle = n`,
`if (gN eq <const>) <jump>`, `jump title N [chapter C]`, `jump titleset N menu [entry root|ptt|audio]`,
`jump vmgm menu N`, `jump menu N`, `jump cell N`, `call menu`, `call vmgm menu N`, `exit`, `resume`,
in `fpc`, `pre`, `post`, `button`. Anything else → error. 128-command limit check stays in `author.rs`.

### Order
1. **Oracle fixtures first** (while dvdauthor.exe exists): `tests/fixtures/dvdauthor/<case>/` with small
   committed inputs (≤ 3 s, low bitrate; reuse `tests/fixtures/spumux/*.mpg` where possible, menus via
   `spumux.rs` from committed PNG masks), the `DvdLayout` as JSON, and per output file an MD5 + the raw
   IFOs (small). Record with `DVDAUTHOR_RECORD=bin/dvdauthor.exe cargo test --test dvdauthor_native`.
   Cases: (a) single title PAL, chapters; (b) merged 2-VOB title; (c) NTSC title; (d) still VMGM menu
   + VTSM menu, buttons, dispatchers, playback options; (e) motion menu with intro cell (oracle output
   post-processed by `strip_intro_buttons`); (f) 2 audio (ac3 + dts) + 2 subpicture streams, 16:9 +
   letterbox; (g) title > 1 GiB is too big for a fixture → synthetic unit test of the split logic only.
2. `dvdvm.rs` + unit tests: every command string of the grep list (and every `ButtonAction::command`
   variant) vs bytes from the recorded IFOs (`dvdunauthor`-free: compare PGC command tables directly).
3. `dvd_mux.rs` scan phase + unit tests (VOBU list, cell split, 0xBE parsing round-trip with `spumux.rs`).
4. `ifo.rs` + `dvd_mux.rs` fixup; iterate on the fixtures until **every file of every case is
   byte-identical**; on mismatch the test prints a field diff (table name + offset), not just MD5.
5. Wire into `dvd.rs`; run all DVD integration tests (with dvdauthor.exe still present but unused).
6. Delete dvdauthor: `bin/dvdauthor.exe`, `bin/dvdauthor-COPYING.txt`, `tools/dvdauthor-win/`
   except what re-records the oracle (move `build.sh` + README to `tools/oracle/` – it also builds the
   spumux oracle), tauri bundle entry, tools.rs, locale keys. Commit.

### Phase 1 verification (must be green before Phase 2)
- `cargo test --test dvdauthor_native` – byte identity, all cases (a)–(f), runs without dvdauthor.exe.
- Unit tests in `dvdvm.rs`, `dvd_mux.rs` (VOB split at 524,287 sectors, SRI table on a synthetic VOBU
  list, BCD time, intro-cell HLI empty), `ifo.rs`.
- Existing: `build_dvd`, `custom_menu`, `menu_intro`, `motion_menu`, `ntsc`, `dvd_audio`, `disc_image`,
  `burn_flow`, `playback_options`, `real_templates`, `native_menu_templates`, `process_hygiene`.
- Extra: `ffprobe` reads every produced VOB without errors; `disc_image` ISO still mounts (existing test).

## Phase 2 – native BDAV muxer

### Design decision: ffmpeg as demuxer
Instead of an own Matroska + H.264 POC/DTS parser, `bd_mux` reads **one ffmpeg pipe per title**:
`ffmpeg -i title.mkv -map 0:v -map 0:a… -c copy -f mpegts pipe:` (+ `-bsf:v h264_mp4toannexb` implicit).
ffmpeg already computes correct PTS/DTS; Rust parses that TS into access units (PES payload + PTS/DTS
per stream) and re-muxes it BD-conformant. LPCM: ffmpeg's mpegts muxer carries `pcm_bluray` natively.
PGS/IGS come from `.sup` files (our own `igs.rs` output and source PGS copies). Process goes through
`Toolchain::ffmpeg_*` (process hygiene).

### New/changed files
| File | Content |
|---|---|
| `src/bd_mux.rs` (new) | TS demux of the ffmpeg pipe; `.sup` reader (PG header → PTS/DTS + segments); H.264 access-unit handling as tsMuxeR `insertSEI, contSPS` does (SPS/PPS before every IDR, buffering-period/pic-timing SEI if missing; libx264 `nal-hrd=vbr` already emits them – check NVENC); AC-3 / DTS (core only, `dca_core` bsf already applied) / LPCM PES with `--new-audio-pes` semantics (stream_id 0xFD + extension as tsMuxeR 2.7.0); PES for PGS (0x90, PID 0x1200+n) and IGS (0x91, PID 0x1400); T-STD scheduler (leak rates/buffer sizes per stream type from tsMuxer), ATS = 27 MHz arrival clock in TP_extra_header, PCR on 0x1001 at ≤ 100 ms (tsMuxeR cadence), PAT/PMT (HDMV registration descriptor, per-stream descriptors as tsMuxeR)/SIT cadence, null-packet-free VBR output, padding to 6144-byte aligned units; collects EP map (IDR → SPN, PTS), STC/ATC info, packet counts. |
| `src/bd_clip.rs` (new) | CLPI (ClipInfo, SequenceInfo, ProgramInfo, CPI/EP_map coarse+fine, ClipMark, ExtensionData as tsMuxeR writes it) and MPLS (AppInfoPlayList, PlayItem IN/OUT, STN_table V/A/PG/IG, PlayListMark incl. intro mark T) writers – port of `CLPIParser::compose*`/`MPLSParser::compose*`. Also takes over what `bd_author.rs` passes as `--custom-chapters`, `--mplsOffset/--m2tsOffset`. |
| `src/pgs.rs` (new) | Text subtitles → PGS display sets (PCS/WDS/PDS/ODS/END, epoch start + clear set per cue) reusing `igs::{rle_encode, rgb_to_ycrcb}` and the segment writer (make `igs::segment` `pub(crate)`). |
| `src/textsub.rs` | `render_cue`/`render_cues` take a target canvas (width, height, scale) instead of `TvStandard` only; DVD call sites pass the old values (pixel goldens must stay identical). BD: 1080p/720p scale. |
| `src/bd.rs`, `src/bd_author.rs` | Replace meta generation + `tools.tsmuxer` with a `MuxJob` struct (inputs, track langs, chapter marks, playlist/clip numbers) → `bd_mux::mux_title` / `mux_menu`. Text subs → `pgs.rs` `.sup` before muxing. Remove `bd_patch` calls. |
| `src/bd_patch.rs` | Deleted; `crc32_mpeg` moves to `bd_mux.rs`. |
| `src/igs.rs` | `build_sup` emits real ICS (0x18) directly (no PCS disguise). |
| `src/tools.rs` | Remove `tsmuxer` field, `is_tsmuxer`, `Toolchain::tsmuxer`; BD no longer has a missing-tool error (remove `bd.rs:506` check). |

### Order
1. **Native PGS text subtitles first, still muxed by tsMuxeR** as `S_HDMV/PGS` (removes `S_TEXT` +
   Arial; keeps the oracle path usable for all later comparisons). Golden PNG test of one 1080p cue.
2. **Oracle fixtures** (while tsMuxeR exists): `tests/fixtures/tsmuxer/<case>/`: small committed MKV
   inputs (≤ 2 s; x264 ultrafast, low res where allowed) and `.sup`s; recorded: full CLPI + MPLS bytes
   and an M2TS summary JSON (per PID: stream type, PES count, PTS/DTS list, PES header bytes of the first
   PES, PMT/SIT bytes, packet count, EP_map PTS list). Record with `TSMUXER_RECORD=bin/tsMuxeR.exe`.
   Cases: 1080p23.976 H.264 + AC-3; 1080i25 fake-interlaced + DTS; 720p59.94 + LPCM 24-bit 5.1;
   576i + 2 audio + 2 PGS; menu clip with IGS (oracle = tsMuxeR + `bd_patch`) incl. intro mark; mixed
   offsets (`mplsOffset=1`).
3. `bd_mux.rs` demux + packetizer, `bd_clip.rs`; iterate until the oracle comparison passes.
4. Wire into `bd.rs`/`bd_author.rs`, delete `bd_patch.rs`, run all BD tests.
5. Delete tsMuxeR (`bin/tsMuxeR.exe`, `bin/tsMuxeR-LICENSE.txt`, tauri bundle, tools.rs, locale keys). Commit.

### Phase 2 verification
- `cargo test --test tsmuxer_native`: CLPI/MPLS **byte-identical except** SPN/ATC/packet-count/rate
  fields (masked by offset list, documented in the test); EP_map same PTS set, every SPN points at a
  packet that starts a PES of PID 0x1011 containing an IDR/I access unit; M2TS summary equal (stream
  types, PES counts, PTS/DTS lists, PMT/SIT bytes).
- New validator tests in `bd_mux.rs`: every packet 192 bytes, file size % 6144 == 0, ATS monotonic,
  continuity counters, PCR interval ≤ 100 ms, PAT/PMT interval, **T-STD buffer simulation** (no
  over-/underflow for TB/MB/EB per stream) on every fixture.
- `ffprobe` on each produced m2ts: stream list + frame count + duration equal to the input MKV.
- Existing: `bluray`, `menu_intro`, `resolution`, `playback_options`, `disc_image`, `burn_flow`.
- **Manual (user):** VLC 3.0.23 on one BD and one AVCHD folder (seek, chapters, menu loop/intro,
  text + PGS subtitles, LPCM/DTS); standalone player stays an open item in STATUS.md.

## Phase 3 – cleanup & final verification
1. Remove leftovers listed above; `.ref-src/` deleted; `.gitignore` entry stays.
2. Update `tauri.conf.json` (bundle resources), `.github/workflows/release.yml` (no dvdauthor/tsMuxeR
   copy step if present), locale files de/en/ru (`engine.tools_dvd`, `engine.tools_bd`,
   `engine.dvdauthor_created` → `engine.authoring_created`, `engine.bd_tsmuxer` → `engine.bd_mux`),
   `CLAUDE.md` (build notes, tool list), `STATUS.md` (architecture, quirks: dvdauthor/tsMuxeR quirks
   become "native behaviour copied from …", known issues: BD text subs no longer Arial).
3. `cargo test --workspace`, `cargo check -p convertx-gui`, Unix check
   (`cargo check --lib --tests --target x86_64-unknown-linux-gnu`), `npm run build` (incl. check-i18n).

## Risks
- Effort: two ports of ~3–4k lines each. Commit after every green step; each phase is its own PR.
- dvdauthor byte identity may hinge on obscure paths (audio gap calc, `vm_optimize`); allowed exit: a
  documented, field-level masked difference only with explanation in the test – never silent.
- tsMuxeR 2.7.0 vs HEAD differences in CLPI/MPLS/PES details → oracle fixtures decide.
- H.264 from NVENC may lack HRD SEI → `insertSEI` port needed (tsMuxeR synthesises it).
- Real-disc playback (hardware players) stays unverified, as today.
