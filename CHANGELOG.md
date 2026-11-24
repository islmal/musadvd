# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.4.0] - 2026-11-24

### Added

- **Start page**: logo, New Project, Load Project and Burn Studio, plus a list of recent projects. Home and New Project buttons in the toolbar.
- **Project settings tab** in the main window: output, encoding, video, audio, subtitle, menu/playback and chapter options moved out of the Settings dialog. Changes apply immediately (size estimate and quality badges follow) and are saved with each project; the Settings dialog keeps app-level options only.
- **Quality readout** on every video card (e.g. "82% · Good") with tips for Fair and Low, and the planned video kbps and bits per pixel in the tooltip.
- **Audio bitrate** setting: DVD 96–448 kbit/s, Blu-ray 192–640 kbit/s, Auto picks 192 for stereo and 448 for 5.1. The size estimate uses the same rates as the encode.
- **Track reordering**: drag handles in the Audio and Subtitles tabs; embedded and external subtitles share one list, so an external `.srt` can become track 1. Batch reorder for selected videos (by language, external files included). The order is the disc's track order and is saved in projects.
- **Subtitles & Audio menus** update when tracks are added, removed or reordered (with Undo).
- **16:9 DVD menus** for widescreen Full D1 projects (automatic, or forced to 4:3/16:9), and a warning with a one-click fix when the videos' frame rates don't match PAL/NTSC.
- **Regional CJK subtitle fonts**: Japanese, Hong Kong, Traditional and Simplified Chinese and Korean subtitles render with that region's glyph forms (from the track language and title hints).
- **Burning**: the Auto speed shows what it will use for the inserted disc ("Auto (6x for this disc)"); erasing is pre-ticked for rewritable discs with data, and Burn asks for confirmation before erasing or writing write-once media.

### Fixed

- **Blu-ray/AVCHD with ASS/SRT subtitles** no longer fails with "invalid .sup format": large subtitle images are split across PGS segments. Every subtitle file is checked before muxing, and errors name the track.
- **Blu-ray playlists** always keep an entry mark, and clips without a usable EP map are rejected instead of producing a broken disc.
- **Work folder** (`.musadvd-work`) is reliably removed after a build (retries while Windows still holds files).
- **Burn confirmation** opens in front of the burn window and closes with it; the speed box no longer shows "Maximum" while the disc is being read.
- **Russian**: chapters are called главы/глава.

## [1.3.0] - 2026-11-22

### Added

- **Multi-select in the video list**: Ctrl/Cmd+click toggles, Shift+click selects a range, Ctrl+A selects all and Esc clears. With two or more selected, a bar offers Merge selected / Unmerge selected; Merge gathers the selection into one block at the first selected title. Remove and Delete remove the whole selection.
- **Title context menu**: right-click a video for Merge all, Unmerge all and Remove – many short clips become one multi-chapter title in two clicks.
- **DVD resolution on the main window**: Full D1 / Half D1 / SIF selector next to the TV standard (same setting as on the settings page; menus stay Full D1).

### Fixed

- **Verify after burning** no longer fails near the end of DVD-Video discs: Windows stopped reading at the file system's end, one sector before the image's closing UDF anchor. Burn Studio's disc read and verify had the same limit.
- **GPU decoding** is no longer given up on MP4 files with broken metadata (ffmpeg's "UDTA parsing failed" warning made the decoder test fail and encode on the CPU).

## [1.2.1] - 2026-11-22

The 1.2.0 release build failed (ffmpeg download error), so 1.2.1 is the first published release with the 1.2.0 changes below.

### Added

- **Main window TV standard**: PAL/NTSC selector next to Medium and Video, in a compact two-column grid above the disc fill bar (same setting as on the settings page).

### Changed

- **Menu editor layout**: the add-element buttons form one compact row with icons; Test Remote sits next to the zoom buttons; the menu list keeps only "+ Menu", the reorder arrows and a "⋯" button, with Duplicate, Copy, Paste and Delete in the row's right-click menu.
- **Menu editor look**: higher-contrast secondary text, pill-shaped view toggles, a TV bezel around the canvas, labelled safe-area guides (Action 5% / Title 10%), and an Elements list with type badges and truncated names instead of a sideways scrollbar.
- **Chapter titles in drafted menus**: the video thumbnail itself opens the title's chapter menu (like a title without chapters plays on click); the caption below is a plain label. Playback still returns to the page showing the thumbnail.

### Fixed

- **Menu headings**: chains of scene/archive tags such as `[DD]+[GOOFY]+[MM]` are stripped from captions and the default heading.
- **Test Remote** follows the disc-wide playback settings (play one after another, loop, return to the title menu) at the end of a title.
- **Release workflow**: the ffmpeg download retries temporary server errors.

## [1.2.0] - 2026-11-22

### Added

- **DVD video copy in the GUI**: a DVD video setting (Encode / Auto / Strict) in the settings and on the main window next to the disc options, saved in `.musadvd` projects. Titles that are already DVD-compliant MPEG-2 are copied unchanged; the title list and pre-flight show "Copied unchanged" or the reason a title is re-encoded.
- **Burn Studio volume labels**: separate ISO 9660 label (A-Z, 0-9, _, 32 characters, cleaned up as you type) and UDF label (free Latin-1 text, 30 characters; UDF 1.02 on DVD/data discs, UDF 2.50 on Blu-ray). An empty UDF label uses the ISO label.

### Fixed

- **Menu highlight on strict players**: every DVD menu page with buttons now starts on button 1 (SetHL_BTN) unless it picks a button itself, so jumping from a page with more buttons no longer leaves SPRM 8 out of range (PgcEdit warning, missing highlight on some players).
- Copied AC-3 audio no longer floods the log with paired "Discontinuity of 1" / "moves backwards by 1" warnings (1/90000 s rounding in ffmpeg's stream copy, resynced anyway).

## [1.1.2] - 2026-11-21

### Added

- **Native disc erasing**: MMC BLANK (quick or full) with progress, for CD-RW and DVD-RW, on Windows and Linux without external tools: a Burn Studio "Erase disc" mode, `musadvd erase`, and automatic erasing of a used DVD-RW in the direct and Linux burn paths when erasing is allowed. DVD+RW and BD-RE are reported as not needing an erase.
- **Menu multi-select**: Ctrl/Shift+click picks several menus in the menu editor for batch apply ("Selected menus") or deleting.

### Fixed

- **DVD size planning**: mux overhead is now reserved per second of playing time (NAV packs), as a share of the stream and per title (IFO files) instead of a flat 2 % of the disc, so long low-bitrate DVDs no longer end up over their target size.
- Subtitle streams no longer flood the log with "pts moves backwards" warnings for overlaps smaller than one subpicture timing unit (harmless, caused by spumux rounding).

## [1.1.1] - 2026-11-21

### Added

- **Menus dropdown** in the menu editor: choose whether a draft builds a title screen, chapter menus and global, per-title or no audio/subtitle menus, and rebuild the menus from there.
- **Copy and paste a whole menu**: "Copy menu" keeps a menu (also across a rebuild or closing the editor); "Paste over" puts it in place of a drafted menu, relinking its buttons by menu name.
- **Paste in place**: elements copied from one menu are pasted on another at the same position and layer; a "Copy" item in the element context menu.
- **Batch apply music**: the music track has its own checkbox, separate from the background.

### Fixed

- The classic grid links the global audio/subtitle menu (it was unreachable without the landing title screen).

## [1.1.0] - 2026-11-21

### Added

- **Menu editor overhaul**: resizable panes and zoom, drag-to-reorder menus with collapsible groups, clipboard image paste, drop on canvas or background, non-destructive crop and "Trim transparency", a searchable live-preview font picker, built-in SVG icons, a new default "Landing Page" theme (the 6-thumbnail grid stays as "Classic Grid") and global or per-title settings menus with disc-wide audio/subtitle choices on DVD and Blu-ray.
- **Text and button look**: Normal / Highlighted / Selected colours, text box fill with opacity, rounded corners and padding, drop shadow (colour, offset, blur) and outline, rendered the same in the editor and on DVD and Blu-ray.
- **Navigation control**: per-button Up/Down/Left/Right overrides and "Auto-execute when focused" on DVD and Blu-ray and in Test Remote.
- **Batch apply**: copy background and audio, typography, navigation icons or header/footer positions from one menu to all menus of the same kind or to every menu.
- **Smart alignment guides and grid**: snapping to the canvas centre, safe areas and neighbouring elements (Alt for free placement) and an optional 8/16 px grid; the title-safe area is shown next to the action-safe area.
- **Selection indicators and new symbols**: pointer, bullet, underline and bracket indicators that only appear while their button is highlighted or selected, plus pill and rectangle frames, chapters, settings, star and arrow icons.
- **Menu play modes**: loop forever, play once and hold, or play once and then run the start button after a timeout; video start offsets are entered as `m:ss`.
- **After playback per title**: continue the disc flow, play the next title, return to the main menu or the title's own menu, or loop the title, on DVD and Blu-ray and in Test Remote.
- **Continuous integration**: a GitHub Actions `CI` workflow builds the frontend, runs the `vitest` suites and runs `cargo check --workspace --tests` and `cargo test --lib` on Windows and Linux for every push and pull request, so Linux-only code (`burner/unix.rs`, `SG_IO`, self-update) is compiled on each commit.
- **Frontend unit tests**: `vitest` suites for `layout.ts` and `project.ts` (`npm test`).
- **Clippy enforcement**: CI runs `cargo clippy --workspace --all-targets -- -D warnings`; existing lints across the workspace were fixed.

### Changed

- **Releases are gated behind CI**: the release workflow runs the CI workflow first, so nightly and versioned builds are only built and published when every check is green.

### Removed

- The always-empty `UNSUPPORTED_CHOICES` plumbing and its locale key.

### Fixed

- The disc image builder (`burner/builder.rs`) returns an error instead of panicking when the source folder changes between detection and planning.
- Poisoned locks in the Windows burner and the i18n catalogs are recovered instead of panicking when a thread exited while holding them.

## [1.0.3] - 2026-11-20

### Added

- **Automatic GPU vendor detection on startup**: both the desktop GUI (on startup and "Reset to defaults") and the CLI (when `--hw-accel` is omitted) probe available hardware encoders (`nvenc`, `qsv`, `amf`) and switch away from an unavailable default/configured vendor automatically instead of always starting on NVIDIA.

### Fixed

- **Paired hardware decode/encode probe & multi-GPU AMD AMF**: `probe_hwaccel_for` now tests candidate `-hwaccel` decoders paired with the selected GPU encoder (`-vf format=yuv420p -c:v <encoder>`) instead of in isolation, preventing mismatched decode/encode failures (such as `d3d11va` + `h264_amf` on multi-GPU Windows systems or `qsv` decode + `-vf` on Linux). On Windows, `h264_amf` also probes explicit `d3d11va:0..3` adapters before falling back to CPU decoding.

## [1.0.2] - 2026-11-20

### Changed

- **Native burning on Linux**: the main window's burn step (and the CLI `burn`) now probe and write the disc with MusaDVD's own MMC writer over `SG_IO`, the engine Burn Studio uses. `growisofs`/`xorriso` are only a fallback when the drive cannot be opened that way, and to erase a used DVD-RW; the `.deb` now only recommends them, the AUR package lists them as optional. Blu-ray media are now recognised on Linux too.
- **Auto write speed** is the same in the main window and Burn Studio: 8x for DVD (instead of the drive maximum, or 4x in Burn Studio), 4x for DVD-9 and Blu-ray; the fastest offered speed up to that, otherwise the slowest offered. Burn time estimates use the same speeds.

### Fixed

- Burn Studio graphs on Linux carry local time instead of UTC and are saved in `~/.local/share/musadvd/graph` instead of the temp folder (wiped on reboot).
- Remaining German texts in the burner ("Maximal" speed label, drive probe notes, book type part).

## [1.0.1] - 2026-11-19

### Changed

- **Faster denoise and picture filters**: when the picture is scaled down (e.g. 1080p to DVD), denoise and brightness/contrast/saturation now run after the scale instead of on the full-size source; denoising was about 4x cheaper in a 1080p-to-DVD test. Upscales keep them in front of the scale, and the live preview follows the same rule.

## [1.0.0] - 2026-11-19

First stable release: the project file format, settings and CLI are now kept compatible within 1.x.

### Added

- **Menu presets with music**: Swiss, Cinema and Midnight now also set bundled background music on every menu without its own sound (`assets/menu-audio/`, Opus 96 kbps, levelled to about -18 LUFS): "Sunlight on the Floorboards" (Swiss), "Porcelain Hours" (Cinema) and "View From The Berth" (Midnight, sci-fi). Shipped in the installer, AppImage/.deb, portable archives and the AUR package.
- **README screenshots** generated from the real app with placeholder videos (`docs/screenshots/`).

- **DVD-Video MPEG-2 smart stream copy** (`encode` default / `auto` / `strict`, CLI `--dvd-video`, `AppSettings::dvd_video`): `encoder::dvd_video_copy` stream-copies sources that are already DVD-compliant MPEG-2 (720x480/704x480 at 29.97/23.976 fps or 720x576/704x576 at 25 fps, `yuv420p`, SDR, <= 9800 kbit/s, no trim/crop/deinterlace/scale/picture-filter overrides). Copied titles still pipe through the native `spumux` stage; their exact byte size is reserved in the bitrate budget (`TitleLoad::copy_bytes`, `calculator::bytes_to_kbit`) and re-encoded titles share the rest. `strict` fails before menus and encoding if any title cannot be copied.
- **Denoise and picture EQ** in `VideoProcessing` / `StreamSelection` (`denoise` via `hqdn3d`; `brightness`, `contrast`, `saturation` via `eq`, all optional and `#[serde(default)]`). They are part of `source_chain`, so the live preview (`preview_filter`) and the final encode use the identical filtergraph. Sliders with Reset in the "Trim & picture" tab; titles with active picture filters are never stream-copied.
- **10-second quality test** (`test_title_quality` Tauri command, "Test Quality (10s)" button): encodes a 10 s sample (at 25 % of the trimmed duration or a chosen timestamp) with the title's planned bitrate and processing, measures PSNR against the source slice (`quality_psnr_args`, `parse_psnr_average`) and shows a verdict (`psnr_verdict`: excellent / good / fair / poor) with a playable sample.
- **Scene-change auto-chapters** (`probe::detect_scene_chapters`, Tauri command `detect_scene_chapters`, "Detect Scenes" button): `select='gt(scene,0.35)',showinfo` inside the title's trim range, cuts closer than 15 s are dropped, capped at the DVD chapter limit.
- **Test Remote mode in the menu editor**: arrow keys, Enter, Backspace and Esc (plus an on-screen D-pad) move focus across buttons using the same links as `author::compute_navigation` (`layout::menu_button_navigation`, Tauri command `menu_navigation`), follow menu actions with back history and preview title/chapter/play-all targets, audio/subtitle choices and VM commands.
- New UI strings in `en`, `de` and `ru` for all of the above.
- **Panda mascot logo**: new app icon set (`src-tauri/icons/`, regenerated from the transparent master `logo/logo.png`, vector trace `logo/logo.svg`), header logo and window favicons.
- **Language picker in the main window header** next to the logo; it changes the same setting as Settings → Language and applies immediately.
- **Verbose updater modal** (`updateDialog.ts`): download progress in %, a step log, and SHA-256 verification (release asset `digest` or `<asset>.sha256`; unverified downloads are refused). NSIS installs go through the updater plugin; AppImage and portable builds are replaced by `self_update.rs`; `.deb` installs only get a hint.
- **AppImage/deb CLI dual mode**: the GUI binary runs the CLI (`musadvd::cli::run`) when started as `musadvd` (argv0 or symlink), with `--cli`, a subcommand, `--help` or `--version`, before GTK starts; the `.deb` ships `/usr/bin/musadvd`.
- **Project files** (`.musadvd` JSON, versioned): Open project (Ctrl+O), Save project (Ctrl+S, Ctrl+Shift+S = save as) in the main window header. A project holds medium, TV standard, copy modes, every title with its tracks, trim/crop, picture filters, chapters, quality weight, merge flags and subtitle style, and the custom menu layouts. Opening re-probes the videos and lists missing files instead of failing. A recovery autosave next to the settings file is offered after an unexpected exit.
- **Subtitle styling and sync** (Subtitles tab, per title): font family, size (%), outline (px), bottom margin (%) and timing offset (ms) for text subtitles, applied to DVD subpictures and Blu-ray PGS alike. Defaults render exactly as before.
- **System fonts**: the menu editor and the subtitle style list every installed font family (`fontdb`) below the curated ones, and the engine resolves them directly.
- **Pre-flight check** before Convert: one dialog lists warnings (disc fill above 99 %, minimum bitrate, stream copies that will fall back to re-encoding, forced deinterlacing with copy, poor quality) and lets you cancel.
- Custom (menu editor) menus use the global menu sound file from Settings for every menu without its own sound, and the layout check verifies with `ffprobe` that menu sound and video files really contain audio/video.

### Changed

- **Clean title captions**: default title labels derived from file names drop ids and rip tags (`cleanTitle`), and draft menu layouts shrink long captions to fit (down to 55 %); labels edited by the user are never touched.

- **Faster two-pass MPEG-2 encoding**: pass 1 is now a statistics-only analysis pass (`-mbd simple -trellis 0 -dia_size 1 -subq 1` with SAD comparisons) while keeping `-passlogfile`, GOP, frame rate, filters and rate control identical to pass 2.
- **Sharper line art on tight DVD bitrates**: pass 2 and single-pass encodes use `-mbd rd` and, unless `--fast` is set, `-trellis 1 -intra_vlc 1`.
- **Strict capacity with quality weights**: a Low-weight title raised to the minimum bitrate can no longer push the disc past 100 %; the shared bitrate is re-solved against the real per-title sizes, and a project that cannot fit even at the minimum is reported as not fitting.
- Integration tests only use the `bin/ffmpeg` built for the current platform, so Linux/WSL runs in a checkout with `bin/ffmpeg.exe` no longer try to start the Windows binary.
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

[Unreleased]: https://github.com/islmal/musadvd/compare/v1.0.2...HEAD
[1.0.2]: https://github.com/islmal/musadvd/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/islmal/musadvd/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/islmal/musadvd/compare/v0.1.5...v1.0.0
[0.1.5]: https://github.com/islmal/musadvd/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/islmal/musadvd/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/islmal/musadvd/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/islmal/musadvd/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/islmal/musadvd/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/islmal/musadvd/releases/tag/v0.1.0
