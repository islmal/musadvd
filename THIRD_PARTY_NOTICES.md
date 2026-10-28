# Third-party notices

ConvertX Rust is licensed under GPL-3.0-or-later (see `LICENSE`). Parts of it derive from or bundle
the following projects.

## dvdauthor (GPL-2.0-or-later)

Source: <https://github.com/ldo/dvdauthor>, commit `fe8fe35` (0.7.2 + fixes).
Copyright (C) 2002 Scott Smith (trckjunky@users.sourceforge.net); the subpicture encoder
(`subgen.c`, `subgen-encode.c`) is Copyright (C) 2002, 2003 Jan Panteltje. The source headers grant
"version 2 of the License, or (at your option) any later version", which allows combining the ports
with GPL-3.0-or-later code.

Rust ports or behaviour re-implementations (output verified byte for byte against dvdauthor):

| File | Derived from |
|------|--------------|
| `src/dvdvm.rs` | `dvdcompile.c`, `dvdvmy.y`, `dvdvml.l` |
| `src/dvd_mux.rs` | `dvdvob.c`, parts of `dvdauthor.c` |
| `src/ifo.rs` | `dvdifo.c`, `dvdpgc.c` |
| `src/spumux.rs` | spumux (`subgen*.c`) behaviour |

## tsMuxer (Apache-2.0)

Source: <https://github.com/justdan96/tsMuxer>, version 2.7.0. Licensed under the Apache License 2.0
(<https://www.apache.org/licenses/LICENSE-2.0>); the upstream repository ships no NOTICE file.
`src/bd_mux.rs` (scheduling) and `src/bd_clip.rs` (CLPI/MPLS layout) follow its behaviour. No
tsMuxer binary is shipped.

## DejaVu Sans (Bitstream Vera licence)

`assets/fonts/DejaVuSans.ttf`, licence text in `assets/fonts/DejaVu-LICENSE.txt`.

## FFmpeg (GPL-3.0)

The installer bundles `ffmpeg.exe` and `ffprobe.exe`, the gyan.dev "full" build (GPL-3.0). Source
code: <https://ffmpeg.org/download.html>. They run as separate processes.

## ConvertX

Not affiliated with VSO Software. ConvertX templates are not included; they are only read from the
user's own ConvertXToDVD installation.
