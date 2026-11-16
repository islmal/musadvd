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

## Menus

- **ConvertX templates**: `template.rs` parses the INIs, `menu.rs` renders a page into a 720x576
  (or 720x480) background plus two 4-colour subpicture masks; `spumux.rs` muxes them.
- **Custom layouts**: the menu editor produces a `CustomLayout` JSON (`layout.rs`,
  `frontend/src/layout.ts`); `layout::validate` enforces DVD limits (rectangular buttons, at most
  36 per menu, no overlaps, at most 128 VM commands per PGC).
- **Blu-ray**: the same `CustomLayout` on a 1920x1080 canvas; templates reach it through
  `bd_template.rs`, which scales pages and translates `ML_*` button locations to `layout::Action`s.
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
ffmpeg child (one token per file). Filters come from `encoder::preview_filter`; a seek starts a
new stream. Used by the "Schnitt & Bild" tab (`frontend/src/edit.ts`).

## Things that are easy to get wrong

See the *Quirks* list in [`../STATUS.md`](../STATUS.md) (section 2); it is kept next to the
decisions that caused them. Examples: trimmed titles always re-encode audio, `TT_SRPT` start
sectors are relative to `VIDEO_TS.IFO`, custom DVD layouts use storage pixels (720 wide,
non-square), Blu-ray has no 1080p25/29.97.
