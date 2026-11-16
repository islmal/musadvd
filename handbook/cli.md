# Command-line interface

Binary `musadvd` (`src/main.rs`, clap). `musadvd --help` and `musadvd <command> --help` are always
authoritative; this page explains what each command is for. Commands that run long can be cancelled
with Ctrl+C (child processes are killed).

| Command | Purpose |
|---|---|
| `template <ini>` | Parse a ConvertX template and print its pages and buttons. |
| `bitrate --disc <d> --duration <t> [--audio kbps] [--overhead kbit]` | Video bitrate calculator for DVD5/DVD9/BD25/BD50 (`calculator.rs`). Duration as seconds or `HH:MM:SS`. |
| `render-menu --template <ini> --output <dir> [--title FILE[@CHAPTERS]] [--page n] [--debug-frames]` | Render one template menu (background + highlight masks) and write the authoring project, for layout checks. |
| `build-dvd --input <files...> --disc <d> --output <dir> (--template <ini> \| --layout <json>)` | Full build: probe, encode, menus, authoring. Produces `<output>/<project>/VIDEO_TS` (DVD) or `BDMV` (Blu-ray/AVCHD). |
| `drives` | List DVD writers and the media in them. |
| `make-iso <VIDEO_TS> <out.iso> [--label]` | Build a burnable UDF 1.02 + ISO 9660 image from a `VIDEO_TS` folder. |
| `burn <folder> [--drive] [--speed] [--label] ...` | Burn a `VIDEO_TS` or `BDMV` folder (image, write, read-back verification). |
| `read-disc`, `verify-disc`, `build-iso`, `iso-label`, `ifo-layerbreak`, `drive-info` | Burn Studio operations (`src/studio_cli.rs`, logic in `src/burner/studio.rs`). |

## `build-dvd` options by theme

- **Disc**: `--disc` (dvd5, dvd9, bd25, bd50, avchd5, avchd9), `--tv pal|ntsc`,
  `--resolution` (DVD: full-d1, half-d1, sif; Blu-ray: hd1080, hd720), `--encoding-profile auto|sp|mp|lp`.
- **Menus**: `--template`, `--layout` (mutually exclusive), `--menu-title`, `--ui-lang`,
  `--motion-menu` with `--motion-duration`, `--motion-silent`, `--menu-audio`,
  `--no-menu-animations`. Blu-rays have no ConvertX templates: without `--layout` they get no menu.
- **Tracks and chapters**: `--audio-lang`, `--sub-lang`, `--no-subs`, `--chapter-minutes`,
  `--audio auto|ac3|lpcm`, `--track-labels smart|language|generic`, `--loudnorm`.
- **Video processing**: `--scale letterbox|panscan|stretch`, `--scale-filter
  bilinear|bicubic|lanczos`, `--deinterlace auto|yadif|off`.
- **Structure**: `--merge 2,3` joins inputs behind one menu entry; `--single-video` (Blu-ray);
  `--project-name`.
- **Performance**: `--jobs N`, `--two-pass`, `--fast`, `--hw-decode`, `--hw-accel
  none|nvidia|intel|amd`, `--no-hw-encode`.
- **Diagnostics**: `--keep-work` keeps `<output>/<project>.musadvd-work/` (intermediate files, logs).

Check `--help` for defaults and value lists.

## Typical sessions

```bash
musadvd bitrate --disc dvd5 --duration 01:45:00 --audio 192
musadvd build-dvd --input movie.mkv --disc dvd5 --template "<template>.ini" --output out
musadvd make-iso out/movie/VIDEO_TS movie.iso --label MOVIE
musadvd burn out/movie --speed auto      # only with the user's explicit go
```
