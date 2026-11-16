# STATUS — MusaDVD

> State as of **2026-11-16**. Architecture/context only; facts: `SYSTEM_STATE.txt`; build rules: `CLAUDE.md`; module docs: `handbook/`.

## 1. Mission & Out-of-Scope

Clean-room Rust DVD, Blu-ray, and AVCHD authoring suite (`musadvd` library + CLI, `musadvd-gui` Tauri 2 desktop app in German/English/Russian, plus **Burn Studio**). Arbitrary videos in → standards-compliant **DVD-Video** (`VIDEO_TS`, ConvertX-style or custom WYSIWYG menus) or **Blu-ray / AVCHD** (`BDMV` + `CERTIFICATE`) out, optionally burnt.

Principles: **clean room w.r.t. ConvertX** (only external `.ini` template files are read); `src/dvdvm.rs`, `src/dvd_mux.rs`, `src/ifo.rs`, and `src/spumux.rs` are **GPL ports/derivations of dvdauthor 0.7.2+**; `src/bd_mux.rs`/`src/bd_clip.rs` derive from **Apache-2.0 tsMuxer 2.7.0** (project license: `GPL-3.0-or-later`). Only `ffmpeg`/`ffprobe` remain external (`growisofs`/`xorriso` on Linux); everything else is native Rust.

**Out of scope / not implemented:** BD-J, Blu-ray pop-up menus, text outlines, template *outro* animation, HEVC/UHD output (HEVC is input only), custom BD size, multi-session, 8.3 camcorder AVCHD layout, MP2/TrueHD/E-AC-3 output, LPCM on DVD-Video, multi-angle, seamless branching, DVD-Audio, CSS, region coding, DVD-R DL layer break (drive default). On Linux: Burn Studio MMC/SPTI, Discovery, Read, and Verify are Windows-only (`CliBurner` supports `drives`, `probe_media`, DVD-RW blanking, and `burn_iso`); macOS is unsupported.

## 2. System Architecture & Data Flow

- **DVD job (5 phases):** probe (`ffprobe`) → plan titles (merge groups) → **[1] menus** (`template.rs`/`menu.rs` or `layout.rs` → native `spumux.rs`) → bitrate from real menu size → **[2] videos** (parallel `ffmpeg` → MPEG-2 + AC-3 piped through native `spumux`) → **[3] authoring** (`author::author_dvd` → `dvdvm`/`dvd_mux`/`ifo` → `VIDEO_TS`) → **[4] ISO** (native UDF 1.02 + ISO 9660 bridge, layer break) → **[5] burn** (Windows IMAPI2 / Linux `growisofs`+`xorriso`).
- **Blu-ray / AVCHD:** `x264` or GPU H.264 (`nvenc`/`qsv`/`amf`, 1080i always `x264`) → MKV → native `bd_mux::mux_title` + `bd_clip` (CLPI/MPLS) → `hdmv.rs`/`bd_menu.rs` + IGS (`igs.rs`) + PGS (`pgs.rs`) → native UDF 2.50 (`udf250.rs`).
- **Burn Studio (`burn_studio.rs` / `studio.rs`):** Windows IMAPI2 or direct MMC/SPTI (`mmcwrite.rs`), Discovery blank-media testing, Read, Verify, and `.ibg` graph telemetry (`graph.rs`).
- **Live player (`preview.rs`):** Loopback HTTP fMP4 proxy from `ffmpeg` (480p ultrafast, shared `source_chain` with deinterlace → HDR tonemap → crop).
- **i18n & config:** Single locale JSONs (`frontend/src/locales/*.json`, `en.json` master and default language) shared by GUI and engine. Settings at `%APPDATA%\musadvd\settings.json` (Windows) or `$XDG_CONFIG_HOME/musadvd/settings.json` (Linux); work dir `<project>.musadvd-work`.
- **GPU decoding:** `hw_decode: true` by default; `probe_hwaccel` tests vendor-preferred then fallback platform decoders (`d3d11va`/`cuda`/`qsv` on Windows; `cuda`/`vaapi`/`qsv` on Linux) via 2 s `ffmpeg` probe.

**Quirks (easy to get wrong):**
- `ffmpeg` `-ss` + stream-copied audio fails to cut start → trimmed titles always re-encode audio.
- `dvd` muxer LPCM causes PTS discontinuities → `lpcm` mode writes AC-3 on DVD.
- `growisofs` reports `%` on `stdout` (not `stderr`) and may leave `/dev/sr0` settling (`ENOMEDIUM` = 123) right after closing session → `unix.rs` drains both pipes and retries `open_for_verify`.
- `tauri.windows.conf.json` holds `../bin/*.exe` and `nsis` targets; `tauri.linux.conf.json` holds `deb`/`appimage` targets so Linux builds do not fail on missing `.exe` globs.

## 3. Roadmap & Known Issues

**Last verified run (2026-11-16):**
- **Windows:** `cargo check --workspace` green, `npm run build` (`check-i18n`, `check-order`, `tsc`, `vite build`) green, `cargo test --lib` 384 passed.
- **Linux Mint 22.3 (i5-1235U Iris Xe + USB `HL-DT-ST BD-RE BU40N`):** `musadvd` + `musadvd-gui` release builds green; `vaapi` GPU decode verified; full DVD-Video authoring + `growisofs` burn to DVD+R verified in **Linux VLC** and on a **PS4**; `burner::unix::tests` + `config::tests` green (`cargo test --lib`: 378/380 passed — 2 Windows-path/font test assumptions fail on Linux).

**Next (prioritised):**
1. Hardware test the 6 DVD-RW discs in Windows **Burn Studio** (Discovery + Verify, Erase + Rewrite) and on Linux (`CliBurner`).
2. Fix the 2 Linux unit-test assumptions (`author::tests::title_spec_cli`, `menu::tests::long_text_is_shrunk_then_ellipsized`).
3. Linux Burn Studio parity (`SG_IO` `ScsiTransport` for Discovery/Read/Verify/Graphs on Linux).
4. Manual checks: QSV/AMF BD encoding on real Intel/AMD hardware, Burn Studio `io.backend = direct`, standalone player for BD/AVCHD/native subtitles.
5. Re-add the Actions secrets (`TAURI_SIGNING_PRIVATE_KEY`, password) and re-enable Actions + the `Release` workflow in the recreated repo before the next release.

**Known issues:** `tests/disc_image.rs::golden_video_ts_image_hash` fails on Windows (hash `9103…` vs expected `5215…`, seen in the 2026-11-16 `cargo test` run; cause not investigated, so later test binaries did not run in that session); 4K HDR live-player proxy and BD menus unverified on standalone players; slim manual-latch drives cannot auto-close via `eject -t` if popped open before verify.

## 4. Milestone Changelog (last 10)
- **2026-11-16 – Repo recreated:** `islmal/musadvd` was deleted and recreated (public) from `main` + `native-authoring` so `refs/pull/*` of the pre-rewrite history are gone; PRs, issues, release `v0.1.0` and Actions history were not kept. Actions is disabled at repo level to prevent release runs. Verified: `git ls-remote origin` shows only the two branches.
- **2026-11-16 – English by default:** engine errors, warnings, burn dialog and CLI texts are English literals (the former German-only ones were translated); default UI/disc language is `en`; `template_dir` setting, the ConvertX tab only appears when templates exist and the menu editor is the default menu mode. Verified: `cargo test --lib` (384), GUI tests, the affected integration tests, `npm run build`; `golden_video_ts_image_hash` still red.
- **2026-11-16 – Handbook consolidation:** `handbook/` holds English developer docs for every module, the CLI, settings/i18n and testing (index: `handbook/README.md`); the old German menu editor HTML/PDF guides were removed. All Markdown files are English by rule (`CLAUDE.md`, global rules). Docs only; no code verification needed.
- **2026-11-16 – MusaDVD rename, Linux hardware validation & multi-platform CI:** Renamed project to **MusaDVD** (`musadvd` / `musadvd-gui`, `de.musadvd.app`, `.musadvd-work`). Verified on Linux Mint 22.3 (i5-1235U + USB BU40N): `vaapi` decode, full DVD-Video authoring + DVD+R burn playable in Linux VLC and PS4. Fixed `unix.rs` stdout+stderr draining & `ENOMEDIUM` verify retry, Linux `$HOME`/`$XDG_CONFIG_HOME` paths, `hw_decode: true` default + cross-vendor fallback, `tauri.{windows,linux}.conf.json` split, `release.yml` (Win `.exe`+`.zip`, Linux `.deb`+`.AppImage`+`.tar.gz` on pushes/tags), and `packaging/aur/PKGBUILD`.
- **2026-10-28 – Public-release prep:** GPL-3.0-or-later (`LICENSE`, `README.md`, `THIRD_PARTY_NOTICES.md`), history rewritten with `git-filter-repo`.
- **2026-10-28 – Project-prefixed work dir:** `<project>.musadvd-work` (was `.convertx-work`).
- **2026-10-28 – DVD closed GOPs + continuous audio PTS:** `-flags +cgop -sc_threshold 1000000000` and `DVD_AUDIO_SYNC` `aresample`.
- **2026-10-28 – GPU vendor selection:** `HwAccelMode` (`none`/`nvidia`/`intel`/`amd`), `BdEncoder::{Qsv,Amf}`, `probe_hw_encoder`.
- **2026-10-28 – Title drag reorder + merge pre-check:** Pointer-event drag (`initTitleDrag`), `mergeMismatches` validation.
- **2026-10-27 – Native authoring cleanup:** `.ref-src/` removed; `dvdauthor.exe`/`tsMuxeR.exe`/`spumux.exe` all replaced by native Rust.
- **2026-10-27 – Native BDAV muxer:** `bd_mux`, `bd_clip`, `pgs` replacing `tsMuxeR.exe` (verified via `tsmuxer_native`).
- **2026-10-27 – Native DVD authoring:** `author::author_dvd` (`dvdvm`, `dvd_mux`, `ifo`) replacing `dvdauthor.exe`.
- **2026-10-27 – Native text subtitles:** `textsub.rs` (DejaVu Sans + `ab_glyph`) replacing `spumux.exe`.

## 5. Dead Ends & Anti-Patterns
- Old e-mail in rewritten history: ✗ mailmap covers it → ✓ needs `git filter-repo --replace-text`.
- GitHub `refs/pull/*/head` survive mirror push: ✗ `push --force --mirror` removes them → ✓ PR refs are read-only; only Support or a new repo removes them.
- Linux burn progress stuck at 15%: ✗ `growisofs` writes `%` to `stderr` → ✓ `growisofs` writes `%` to `stdout`; drain both `stdout` and `stderr`.
- Tauri Linux build fails on `../bin/*.exe`: ✗ shared `tauri.conf.json` resources → ✓ move `.exe` glob to `tauri.windows.conf.json`.
