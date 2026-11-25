# Engine modules (`src/`)

One entry per module of the `musadvd` library crate. Grouped by role; see
[architecture.md](architecture.md) for how they fit together. Origin notes: **dvdauthor** ports are
GPL-2.0, **tsMuxer** ports are Apache-2.0 (see `THIRD_PARTY_NOTICES.md`).

## Orchestration

| Module | Purpose |
|---|---|
| `dvd.rs` | End-to-end DVD build (`build_dvd`): probe, plan, thumbnails, menus, bitrate, encode, author. Owns `BuildOptions`, `BuildError`, merge groups (`groups`, `check_title_count`), mixed subtitle order (`subtitle_order`), the size plan (`bitrate_plan_at`, `min_video_kbps`, `hw_reserve_mb`, `weighted_video_kbps`; see architecture "Size planning"), work directory handling (`work_dir`, `keep_work`, `remove_dir_retry`), layer-break-aware output. Hands over to `bd.rs` for Blu-ray/AVCHD. |
| `bd.rs` | Blu-ray / AVCHD build (`build_bd`): per-title H.264 encode (software or hardware, `title_encoders`), one mux run per title, menus, `index.bdmv`; AVCHD form of the output (`write_navigation(.., avchd)`, `avchd_versions`). |
| `eta.rs` | Remaining-time estimates over the five job phases (menus, encode, authoring, image, burn) from telemetry and measured throughputs. |
| `calculator.rs` | Video bitrate for DVD5/DVD9/BD targets: `(target_MB*8192 - audio_kbps*duration - overhead)/duration`, clamped to the mux limit (`calculate_floored` takes a frame-size floor); `bytes_to_kbit` converts fixed (copied) sizes into the same unit. |
| `config.rs` | `AppSettings` and the Burn Studio settings tree, persisted as JSON (`%APPDATA%\convertx-rust\settings.json`); `sanitized()` clamps values; `title_resolution(disc)` picks the DVD or Blu-ray frame size; `apply_to` fills `BuildOptions`. See [settings-i18n.md](settings-i18n.md). |
| `i18n.rs` | Engine-side translation (`tr`, `tf!`, `UiLang`) from the shared locale files. |
| `tools.rs` | Locates and runs ffmpeg/ffprobe (`Toolchain`, `tool_command`, `blocking_tool_command`), hardware encoder/decoder probes, piped ffmpeg helpers. The only module allowed to spawn processes. |

## Input analysis and encoding

| Module | Purpose |
|---|---|
| `probe.rs` | `ffprobe` JSON to `MediaInfo`: streams, chapters, HDR flag, track labels (`audio_labels`, `subtitle_labels`, `clean_tag`), scene-cut chapters (`detect_scene_chapters`, `parse_scene_timestamps`, `filter_scene_chapters`). |
| `encoder.rs` | ffmpeg wrapper: source to DVD-compliant MPEG-2 + AC-3 program stream. Video processing (`VideoProcessing`, `fit_chain`, `source_chain`), audio planning (`plan_audio`, AC-3 rate from the setting `ac3_settings` / `snap_ac3_kbps`, Blu-ray copy rules `bd_audio_copy` / `plan_audio_bd` with `BdAudioMode`), Blu-ray video copy (`bd_video_copy`, `BdVideoMode`), DVD MPEG-2 video copy (`dvd_video_copy`, `DvdVideoMode`, `DvdVideoCopy`, `is_dvd_native_format`, `EncodeJob::copy_video`), picture filters (`VideoProcessing::{denoise, brightness, contrast, saturation}`, `picture_filters`, shared by `source_chain` and `preview_filter`), trim/crop, loudnorm, two-pass (`video_codec_args(analysis)`: fast pass 1, `-mbd rd` / `-trellis 1` / `-intra_vlc 1` otherwise), quality test helpers (`quality_psnr_args`, `parse_psnr_average`, `psnr_verdict`), live-player filter (`preview_filter`). |

## DVD authoring (native replacement of dvdauthor / spumux)

| Module | Purpose |
|---|---|
| `author.rs` | `author_dvd`: builds the disc structure (VMGM plus one titleset per title) and drives the three modules below; `dvdauthor_xml` only feeds reports and `render-menu`. |
| `dvdvm.rs` | VM command compiler (`<pre>`, `<post>`, `<button>`, `<fpc>` strings to 8-byte instructions, incl. `vm_optimize`). Port of dvdauthor `dvdcompile.c`. |
| `dvd_mux.rs` | VOB scan, cell/program marking, NAV pack fixing (`fix_vobus`). Port of dvdauthor `dvdvob.c`. |
| `ifo.rs` | `VIDEO_TS.IFO`, `VTS_nn_0.IFO`, `.BUP`, all tables and PGCs. Port of dvdauthor `dvdifo.c`/`dvdpgc.c`. |
| `spumux.rs` | Subpicture muxer: palette PNGs to SPU units (RLE + SP_DCSQ) multiplexed into the program stream with `dvdauthor-data` sectors. Replaces `spumux -m dvd`. |

## Menus

| Module | Purpose |
|---|---|
| `template.rs` | Parser for ConvertX Delphi-style INIs (BOM, CRLF, comma decimals, nested collections, colour syntax). |
| `menu.rs` | Template page renderer: background PNG plus highlight/select masks; `render_page` for intro sprites. |
| `layout.rs` | `CustomLayout` model (menus, elements: text/image/video/rect, actions), `validate` / `validate_bd`, the DVD renderer `render_custom_page`, shared helpers (`rect_image`, `frame_box`, `fit_image`), `menu_button_navigation` (remote links through `author::compute_navigation`, used by the editor's Test Remote mode). |
| `menu_anim.rs` | Intro keyframe interpolation and `Sprite` compositing for DVD and BD menu intros. |
| `bd_template.rs` | ConvertX templates on Blu-ray: `layout_from_template` scales pages to 1920x1080 and translates `ML_*` buttons to actions. |
| `bd_menu.rs` | HDMV navigation (`plan`, `plan_without_menus`; movie titles never `JumpObject`, return menu in `GPR8`, per-title stream choices applied before `PlayPL`) and rendering of each menu to a clip description plus IGS. |
| `igs.rs` | Interactive Graphics segment writer (ICS, PDS, ODS, END) in `.sup` layout; `ods_segments` splits objects over 64 KB into ODS fragments (also used by `pgs.rs`). |
| `hdmv.rs` | HDMV command encoding (incl. register-form `set_stream_regs`), `index.bdmv`, `MovieObject.bdmv`; `avchd_index` gives the AVCHD index (version `0100` plus tsMuxeR's `IDEX` ExtensionData, `AVCHD_INDEX_EXTENSION`). |

## Subtitles

| Module | Purpose |
|---|---|
| `textsub.rs` | SRT parser and native renderer (bundled DejaVu Sans, 2 px outline, bidi reordering for RTL scripts). Also `Canvas::hd` for PGS; `SubtitleStyle.lang` selects regional CJK fonts. |
| `fonts.rs` | System font lookup; `cjk_region(lang, title)` maps a track to Japanese, Hong Kong, Traditional/Simplified Chinese or Korean glyph forms (title hints like 繁體, TC, 简体, HK), and `preferred_families` orders the fonts for it. |
| `subpic.rs` | Bitmap subtitles (PGS, VobSub) to DVD subpictures; reduces to transparent + 3 colours; emits `<spu>` XML for `spumux`. |
| `pgs.rs` | Text subtitles as Blu-ray Presentation Graphics (one epoch per cue; overlapping cues therefore replace each other on screen). |

## Blu-ray muxing

| Module | Purpose |
|---|---|
| `bd_author.rs` | Turns encoded `title_NN.mkv` and subtitle files into `MuxJob`s; keeps track layout identical across titles. |
| `bd_mux.rs` | Native BDAV muxer: m2ts with PCR/PAT/PMT/SIT, ATS interpolation, frame-grid timestamps; `validate_sup` checks subtitle files before muxing. Derived from tsMuxeR 2.7.0. |
| `bd_clip.rs` | CLPI and MPLS writers (version 0200; `bd.rs` sets `0100` for AVCHD, the structure is the same). Derived from tsMuxeR 2.7.0. |

## Binary entry points

| File | Purpose |
|---|---|
| `lib.rs` | Module declarations. |
| `main.rs` | CLI definition (clap) and command dispatch, see [cli.md](cli.md). |
| `studio_cli.rs` | Burn Studio sub-commands as thin wrappers over `burner::studio`. |
