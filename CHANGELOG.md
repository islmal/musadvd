# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

MusaDVD is pre-1.0 (`0.x`): features and settings may still change between releases. Nothing below is tagged or released yet.

### Added

- **DVD-Video MPEG-2 smart stream copy** (`encode` default / `auto` / `strict`, CLI `--dvd-video`, `AppSettings::dvd_video`): `encoder::dvd_video_copy` stream-copies sources that are already DVD-compliant MPEG-2 (720x480/704x480 at 29.97/23.976 fps or 720x576/704x576 at 25 fps, `yuv420p`, SDR, <= 9800 kbit/s, no trim/crop/deinterlace/scale/picture-filter overrides). Copied titles still pipe through the native `spumux` stage; their exact byte size is reserved in the bitrate budget (`TitleLoad::copy_bytes`, `calculator::bytes_to_kbit`) and re-encoded titles share the rest. `strict` fails before menus and encoding if any title cannot be copied.
- **Denoise and picture EQ** in `VideoProcessing` / `StreamSelection` (`denoise` via `hqdn3d`; `brightness`, `contrast`, `saturation` via `eq`, all optional and `#[serde(default)]`). They are part of `source_chain`, so the live preview (`preview_filter`) and the final encode use the identical filtergraph. Sliders with Reset in the "Schnitt & Bild" tab; titles with active picture filters are never stream-copied.
- **10-second quality test** (`test_title_quality` Tauri command, "Test Quality (10s)" button): encodes a 10 s sample (at 25 % of the trimmed duration or a chosen timestamp) with the title's planned bitrate and processing, measures PSNR against the source slice (`quality_psnr_args`, `parse_psnr_average`) and shows a verdict (`psnr_verdict`: excellent / good / fair / poor) with a playable sample.
- **Scene-change auto-chapters** (`probe::detect_scene_chapters`, Tauri command `detect_scene_chapters`, "Detect Scenes" button): `select='gt(scene,0.35)',showinfo` inside the title's trim range, cuts closer than 15 s are dropped, capped at the DVD chapter limit.
- **Test Remote mode in the menu editor**: arrow keys, Enter, Backspace and Esc (plus an on-screen D-pad) move focus across buttons using the same links as `author::compute_navigation` (`layout::menu_button_navigation`, Tauri command `menu_navigation`), follow menu actions with back history and preview title/chapter/play-all targets, audio/subtitle choices and VM commands.
- New UI strings in `en`, `de` and `ru` for all of the above.

### Changed

- **Faster two-pass MPEG-2 encoding**: pass 1 is now a statistics-only analysis pass (`-mbd simple -trellis 0 -dia_size 1 -subq 1` with SAD comparisons) while keeping `-passlogfile`, GOP, frame rate, filters and rate control identical to pass 2.
- **Sharper line art on tight DVD bitrates**: pass 2 and single-pass encodes use `-mbd rd` and, unless `--fast` is set, `-trellis 1 -intra_vlc 1`.
- MPEG-2 sources reporting the VBR bitrate sentinel (`104857200` bit/s) now fall back to the file-size bitrate.

## [0.1.5] - 2026-11-19

### Added

- **Blu-ray audio copy mode** (`auto` / `encode` / `strict`, CLI `--bd-audio`, GUI footer selector, per-track Copy/Re-encode badges, and strict pre-build gate): passes through compliant AC-3, DTS (including DTS-HD core extraction), and lossless integer PCM/FLAC (≤ 24-bit) repacked as LPCM with accurate wire-rate sizing.
- **Menu editor rectangles and design presets**: `Rect` elements (fill and optional border) supported in both DVD and Blu-ray renderers and the WYSIWYG editor, plus one-click `swiss`, `cinema`, and `midnight` presets (single undo step).
- Windows NSIS installer now bundles the `musadvd.exe` CLI binary alongside the desktop GUI.

### Changed

- Standardized the `--audio` multichannel label to English (`Multichannel`).

### Fixed

- **Blu-ray / PS4 HDMV playback compatibility**: fixed still menus closing after ~1 s (infinite still mode with loop), fixed crashes at title end and with the Options overlay open (`JumpTitle 0` + `GPR8` menu dispatch instead of `JumpObject` from movie titles), fixed settings-menu audio/subtitle choices being ignored (per-title register-form `SetStream` applied before `PlayPL`), ensured empty PGS tracks carry the title's frame rate, and improved mixed-width D-pad button navigation.
- **Non-ASCII subtitle garbling**: fixed UTF-8 byte-by-byte corruption when extracting subtitle alignment tags so German umlauts (`ä, ö, ü, ß`), Cyrillic, French accents, CJK, and RTL text render cleanly in DVD and PGS subtitles.
- **Malformed ISO verification hang**: added visited-directory LBA cycle/DAG detection and extent-bounds checking in the ISO 9660 reader so corrupted disc images fail cleanly instead of hanging verification or over-allocating memory.

### Security

- **Release `bin/` lookup hardening**: restricted working-directory `./bin/` lookup for `ffmpeg`/`ffprobe` to debug builds only, preventing binary planting on Windows and Linux; release builds check `<exe-dir>/bin` and `PATH`.
- **UNC and path traversal protection**: rejected UNC/device paths (`\\server\share`, `//server/share`, `\\?\...`, `\\.\...`), drive prefixes, and `..`/symlink escapes when resolving template fonts/resources, `.cue`/`.dvd`/`.mds` sidecar image targets, and GUI template thumbnail previews.

## [0.1.4] - 2026-11-17

### Fixed

- Linux AppImage live-preview H.264/AAC decoding by installing `gstreamer1.0-libav`, `gstreamer1.0-plugins-good`, and `gstreamer1.0-plugins-bad` on the Linux CI runner so they are bundled into the AppImage.

## [0.1.3] - 2026-11-17

### Changed

- Settings migration: legacy `bd_hw_encode = false` automatically migrates to `hw_accel_mode = none`.
- Tuned parallel job limits based on benchmarks (`HW_MAX_JOBS` reduced from 4 to 2; `MOTION_MENU_MAX_JOBS` kept at 2).

### Fixed

- Removed Windows-specific path and font assumptions from unit tests (`title_spec_cli` and long-text menu wrapping) for cross-platform test consistency.

## [0.1.2] - 2026-11-17

### Added

- **Blu-ray video copy mode** (`encode` / `auto` / `strict`, CLI `--bd-video`, GUI footer selector, per-title Video tab for resolution/frame-rate/copy overrides, Copy/Re-encode badges, and app version badge next to the logo).
- **Blu-ray timing repair and copy scan**: build-time VBV buffer and 1 s IDR-gap validation for copy candidates, plus automatic SPS VUI/HRD timing and buffering-period/picture-timing SEI insertion for copied H.264 streams lacking Blu-ray timing.
- **Per-title quality weight** (Low / Normal / High) and a **"Fit to disc"** button that selectively re-encodes the largest copied titles until the project fits.
- **AVCHD 1440×1080** video copy support.
- **Linux Burn Studio over `SG_IO`**: drive details, CSS detection, Discovery blank-media tests, Read, Verify, and graph CPU telemetry via `/proc/stat`.
- **Linux update-available notice** in the status bar via the GitHub Releases API (for `.deb` and portable users).

### Changed

- Menu editor uses real chapter start times and clip start positions; motion-menu toggle is shown only for DVD ConvertX template menus.
- Copy verdicts follow the active TV standard/resolution, and DTS-HD MA bitrate is measured directly from packet sizes.

### Fixed

- IMAPI2 layer-break setup retries exclusive drive access up to 3 times.

## [0.1.1] - 2026-11-16

### Added

- Arch Linux `packaging/aur/PKGBUILD` (`musadvd-bin`).

### Changed

- Menu editor project videos automatically follow their button's title/chapter/menu target (and vice versa).
- Trim tab displays a poster frame before loading video playback, and stopping the live preview runs asynchronously.

### Fixed

- Linux AppImage freeze when opening the Trim tab by bundling the GStreamer media framework and adding GStreamer runtime dependencies to `.deb` and `PKGBUILD`.
- Missing UI translations in Linux `.deb`, AppImage, and Arch packages by searching `../lib/MusaDVD/locales` and `../share/musadvd/locales`.

## [0.1.0] - 2026-11-16

### Added

- Initial public release of **MusaDVD** (`musadvd` CLI and `musadvd-gui` Tauri 2 desktop app, GPL-3.0-or-later) with English (default), German, and Russian runtime-loaded locales.
- Native Rust DVD-Video (`VIDEO_TS`), Blu-ray, and AVCHD (`BDMV` + `CERTIFICATE`) authoring, muxing, subpicture/PGS text subtitle rendering (markup formatting, CJK font fallback, RTL BiDi, and Arabic cursive shaping), and UDF 1.02 + ISO 9660 / UDF 2.50 image writers — requiring only `ffmpeg`/`ffprobe` externally (`growisofs`/`xorriso` on Linux).
- Built-in WYSIWYG Menu Editor with canvas timeline, intro keyframes, audio waveform, undo history, external video backgrounds, and ConvertX `.ini` template import (with configurable template directory).
- **Burn Studio**: disc image Read, Build, Verify, and Write (Windows IMAPI2 / direct SPTI fallback), Discovery blank-media testing, `.ibg` telemetry graphs, layer-break tools, CSS detection, and safe 4x auto write speed.
- GPU-accelerated decoding/encoding (`HwAccelMode`), live preview proxy with frame stepping, smart track labels, drag-and-drop title reordering, merge groups, and automated Windows (NSIS installer with in-app updater + portable ZIP) and Linux (`.deb`, `.AppImage`, portable `.tar.gz`) release builds.

[Unreleased]: https://github.com/islmal/musadvd/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/islmal/musadvd/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/islmal/musadvd/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/islmal/musadvd/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/islmal/musadvd/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/islmal/musadvd/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/islmal/musadvd/releases/tag/v0.1.0
