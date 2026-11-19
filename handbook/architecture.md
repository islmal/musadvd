# Architecture

MusaDVD turns video files into a standards-compliant **DVD-Video** (`VIDEO_TS`), **Blu-ray** or
**AVCHD** (`BDMV`) structure with menus, optionally burns it, and ships a Burn Studio for image
work. It is a Cargo workspace:

| Part | Path | Role |
|---|---|---|
| Engine library | `src/` | Everything: probing, encoding, menus, authoring, muxing, image writing, burning |
| CLI | `src/main.rs`, `src/studio_cli.rs` | `musadvd` binary, see [cli.md](cli.md) |
| Burn logic | `src/burner/` | Disc images, drive access, Burn Studio operations, see [modules-burner.md](modules-burner.md) |
| Desktop GUI backend | `src-tauri/` | Tauri 2 commands and events wrapping the engine |
| GUI frontend | `frontend/` | Vanilla TypeScript, built by Vite into `dist/` |
| Tests | `tests/` | Integration tests, fixtures recorded from the original tools |

The only external programs are `ffmpeg` and `ffprobe` (from `bin/`, then the working directory,
then `PATH`). Everything else (menu renderer, subpicture muxer, DVD authoring, BDAV muxer, UDF/ISO
writers, burn engines) is Rust in this repository.

## Principles

- **Clean room with respect to ConvertX.** Only its template `.ini` files are read at runtime
  (from the user's own installation); none of its code is used.
- **Ported, not invented, where bytes must match.** `dvdvm.rs`, `dvd_mux.rs`, `ifo.rs` and
  `spumux.rs` follow dvdauthor/spumux (GPL-2.0); `bd_mux.rs` and `bd_clip.rs` follow tsMuxeR 2.7.0
  (Apache-2.0). Their output is checked byte for byte against recorded output of the original
  tools (`tests/dvdauthor_native.rs`, `tests/spumux_native.rs`, `tests/tsmuxer_native.rs`). The
  project is GPL-3.0-or-later; see `THIRD_PARTY_NOTICES.md`.
- **Deterministic authoring.** All disc addresses are planned before anything is written (IFO
  tables, UDF/ISO layout, layer break), so the output is reproducible and testable.
- **Native on Windows.** Tools come from `bin/` first; every child process is started through
  `tools::tool_command` with `CREATE_NO_WINDOW` (enforced by `tests/process_hygiene.rs`).
- **Honest UI.** Unsupported settings are shown but disabled with the reason; every long step has
  progress and an ETA; cancel kills all child processes.

## DVD pipeline (five phases)

`eta::JobPhase` drives the "Phase n/5" display. Phases 1–3 are `dvd::build_dvd`; 4–5 are the burn
dialog (`src-tauri/src/burn.rs`) and `burner::burn_video_ts`, shared by GUI and CLI.

```text
probe (ffprobe JSON, probe.rs)
  -> plan titles (one titleset per menu entry = merge group)
  -> [1] menus      template.rs + menu.rs  (ConvertX template)
                    layout.rs              (custom layout from the menu editor)
                    masks -> spumux.rs     (subpicture muxing, native)
  -> bitrate from the real menu size (calculator.rs)
  -> [2] videos     encoder.rs: one ffmpeg run -> MPEG-2 + AC-3 program stream,
                    piped through one native subtitle stage per stream (textsub/subpic -> spumux)
  -> [3] authoring  author.rs -> dvdvm.rs / dvd_mux.rs / ifo.rs -> VIDEO_TS
  -> [4] ISO        burner::create_disc_image (UDF 1.02 + ISO 9660 bridge, layer break)
  -> [5] burn       IMAPI2 (Windows) / growisofs or xorriso (Unix)
```

Output layout: `<output>/<project>/VIDEO_TS/`; temporary files in `<output>/<project>.musadvd-work/`
(removed after the build unless `keep_work`).

**Video copy (`DvdVideoMode`)**: with `auto`/`strict`, `encoder::dvd_video_copy` decides per title
whether compliant MPEG-2 is stream-copied (`-c:v copy`, still piped through the native subtitle
stage). `bitrate_plan` reserves a copied title's exact byte size (`TitleLoad::copy_bytes`) and
shares the rest among re-encoded titles; `strict` fails before menus and encoding. Two-pass
encodes run a fast statistics-only pass 1 with the same GOP and rate control as pass 2.

## Blu-ray / AVCHD pipeline

`build_dvd` hands over to `bd::build_bd` for `DiscType::Bd25/Bd50/Avchd5/Avchd9`.

```text
per title: libx264 or hardware H.264 (h264_nvenc / h264_qsv / h264_amf) -> title_NN.mkv
  -> bd_author.rs builds one MuxJob per title
  -> bd_mux.rs   ffmpeg `-c copy` -> MPEG-TS pipe -> re-packetised BDAV m2ts
                 bd_clip.rs writes CLPI + MPLS (own playlist and clip per title, own frame rate)
  -> menus       layout.rs / bd_template.rs -> bd_menu.rs (HDMV movie objects)
                 igs.rs (Interactive Graphics buttons), pgs.rs (text subtitles)
  -> hdmv.rs     index.bdmv + MovieObject.bdmv
  -> burner/udf250.rs  UDF 2.50 image with metadata partition (no ISO bridge)
```

AVCHD is the same pipeline on DVD-5/9 media with AVCHD limits (BDMV on DVD media, not the 8.3
camcorder layout). Only HDMV is supported: no BD-J, no pop-up menus.

**Audio copy (`BdAudioMode`)**: `encoder::bd_audio_copy` is the single rule set (returns the copy
plan or an English reason); `plan_audio_bd` applies the mode, and `bd.rs` runs the strict check
before `bitrate_plan`, where copied tracks count at their real size. LPCM is always sized at its
wire rate: `rate_khz * bits * (ch + ch % 2)` (odd channel counts are padded). DTS-HD is copied
through `dca_core` (stream type 0x82); the muxer has no DTS-HD (0x85/0x86) stream type.

**HDMV navigation rules** (`bd_menu::plan`; found on a PS4, VLC/libbluray accepts all violations):
- First Play and Top Menu are interactive titles; every disc title is a movie title. A movie
  title never uses `JumpObject`: it ends with `JumpTitle 0` (Top Menu). To return to another menu
  it first stores that menu's object in `GPR8`; Top Menu jumps there and clears it.
- Still menus use `still_mode = 2` (infinite, `still_time` 0) and still loop
  `SetButtonPage(1); PlayPL; Goto 1`, so the VM never falls off the object. The ICS has no
  default button; `SetButtonPage` selects it and PSR10 keeps the focus over restarts.
- Audio/subtitle buttons of a settings menu only store the choice (`GPR16 + 2*(t-1)` audio
  number, `+1` PG number or `0xFFFF` off). The title object applies it right before `PlayPL`
  with register-form `SetStream`, whose flags are immediate and whose number fields name the
  GPRs. A `SetStream` inside the menu is ignored by players that check it against the menu
  playlist (one audio stream, no PG).

Register map: `GPR1` chapter mark, `GPR2` play-all flag, `GPR3`-`GPR5`/`GPR7` intro dispatcher,
`GPR6` scratch, `GPR8` return menu, `GPR16+` per-title stream choices.

## Menus

- **ConvertX templates**: `template.rs` parses the INIs, `menu.rs` renders a page into a 720x576
  (or 720x480) background plus two 4-colour subpicture masks; `spumux.rs` muxes them.
- **Custom layouts**: the menu editor produces a `CustomLayout` JSON (`layout.rs`,
  `frontend/src/layout.ts`); `layout::validate` enforces DVD limits (rectangular buttons, at most
  36 per menu, no overlaps, at most 128 VM commands per PGC).
- **Blu-ray**: the same `CustomLayout` on a 1920x1080 canvas; templates reach it through
  `bd_template.rs`, which scales pages and translates `ML_*` button locations to `layout::Action`s.
- **Elements**: text, image, video and `rect` (fill, optional border in canvas units, scaled like
  font sizes). Both renderers (DVD `render_custom_page`, Blu-ray `render_page`) draw a rect
  through the shared `layout::rect_image`; a rect with an action is a button with the frame
  highlight.
- **Presets**: `draftLayout(titles, tv, heading, preset?)` in `frontend/src/layout.ts` restyles the
  draft as `swiss`, `cinema` or `midnight` (flat backgrounds, rects, text, highlight colours; no
  bundled image files). The editor's preset picker applies one as a single undo step.
- **Intros / motion menus**: `menu_anim.rs` interpolates keyframes and composites sprites in
  memory (rayon); frames are piped to the encoder and never touch disk. A page is one clip
  `[intro | loop]` with marks;
  see `frontend/src/menuEditor.ts` and `layout.rs` for the model.

## Burn Studio

The `burn-studio` window and the CLI `read-disc`, `verify-disc`, `build-iso`, `iso-label`,
`ifo-layerbreak`, `drive-info` commands share `burner/studio.rs`. Sources (file, `.bin`, drive,
on-the-fly image through `ringbuf.rs`) feed a backend: `imapi` (default) or `direct`
(`mmcwrite.rs`, own MMC commands over SPTI). Telemetry is sampled in `graph.rs` and can be saved as
`.ibg`. The Tauri side (`burn_studio.rs`) only wraps commands and events.

## Live player

`src-tauri/src/preview.rs` is a loopback HTTP server that streams a 480p H.264 fMP4 proxy from one
ffmpeg child (one token per file). Filters come from `encoder::preview_filter` (same `source_chain` as the encode, including denoise and picture EQ); a seek starts a
new stream. Used by the "Schnitt & Bild" tab (`frontend/src/edit.ts`).

## Things that are easy to get wrong

See the *Quirks* list in [`../STATUS.md`](../STATUS.md) (section 2); it is kept next to the
decisions that caused them. Examples: trimmed titles always re-encode audio, `TT_SRPT` start
sectors are relative to `VIDEO_TS.IFO`, custom DVD layouts use storage pixels (720 wide,
non-square), Blu-ray has no 1080p25/29.97.
