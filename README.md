# MusaDVD

DVD, Blu-ray and AVCHD authoring in Rust: a desktop app (Tauri; German, English, Russian) and a
CLI that turn video files into a menu-driven disc structure, an image, or a burned disc.

Not affiliated with VSO Software. ConvertX templates are not included; they are only read from a
user's own ConvertXToDVD installation.

## Features

- DVD-Video, Blu-ray and AVCHD output, native muxing and IFO/BDMV writers (no dvdauthor/tsMuxeR needed)
- Menus from ConvertX templates or custom layouts
- Burn Studio: read, build, verify and burn images (UDF 1.02 + ISO 9660), layer-break tools
- ffmpeg-based encoding, optional NVENC / GPU vendor selection

## Status and limits

Windows first; output has only been checked in VLC. Authoring is limited to what the engine
emits; see [STATUS.md](STATUS.md) for the out-of-scope list and known issues.

## Download

Prebuilt Windows binaries are published on [GitHub Releases](https://github.com/islmal/musadvd/releases):

- **Installer (`.exe`)** — standard Windows setup with auto-updater
- **Portable (`.zip`)** — extract and run `musadvd-gui.exe` or `musadvd.exe` (includes `ffmpeg`/`ffprobe`)

## Build

Copy `ffmpeg.exe` and `ffprobe.exe` (gyan.dev full build) into `bin/`, then:

```
cargo build --release                                                    # CLI
npm run build                                                            # frontend
cargo build --release -p musadvd-gui --features tauri/custom-protocol   # GUI
cargo test --workspace
```

## CLI

```
musadvd drives
musadvd --help
```

## Documentation

All handbooks and module documentation are in [handbook/](handbook/README.md): architecture, engine,
burner and GUI modules, CLI, settings and testing, plus the German menu editor user guides.

## Licence & credits

GPL-3.0-or-later, see [LICENSE](LICENSE). Derived and bundled components (dvdauthor, tsMuxer,
DejaVu, FFmpeg): [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
