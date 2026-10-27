# Native dvdauthor/spumux for Windows

`bin/dvdauthor.exe` (and `target/spumux-oracle/spumux.exe`, see below) are built from the official sources
(<https://github.com/ldo/dvdauthor>, commit `fe8fe35`, 0.7.2 + fixes) with MSYS2 UCRT64 and are
statically linked: they only import Windows system DLLs, so no extra DLLs are needed in `bin/`.

## Rebuild

```sh
scoop install msys2
# in an MSYS2 UCRT64 shell:
pacman -S --needed git autoconf automake libtool make flex bison gettext-devel \
  mingw-w64-ucrt-x86_64-{gcc,pkgconf,libxml2,libpng,freetype,fribidi,libiconv,zlib,libdvdread}
bash tools/oracle/build.sh
```

## Windows port

dvdauthor is POSIX code; the port needs no source changes, only shims passed at `make` time:

- `wincompat.h` (force-included): `mkdir(path, mode)`, `fsync`, `bzero`, and **binary mode for
  `open()`/`fopen()`/stdin/stdout**. Without it the CRT's text mode corrupts every MPEG stream.
- `include/langinfo.h`: `nl_langinfo(CODESET)` returns `UTF-8`.
- `include/netinet/in.h`: `htonl`/`htons`/`ntohl`/`ntohs`.

`spumux` is not shipped or called by the product: subpicture muxing and text subtitle rendering are
native (`src/spumux.rs`, `src/textsub.rs`). The binary only re-records the byte-identity fixtures of
`tests/spumux_native.rs` (`SPUMUX_RECORD=target/spumux-oracle/spumux.exe cargo test --test spumux_native`).

Only `dvdauthor` and `spumux` are built; `spuunmux`, `mpeg2desc` and `dvdunauthor` are not needed.

## License

dvdauthor is GPL-2.0 (`bin/dvdauthor-COPYING.txt`). When distributing the binaries, ship or offer
the corresponding source: the upstream commit above plus this directory.

## tsMuxeR (BDAV oracle)

The Blu-ray muxer (`src/bd_mux.rs`, `src/bd_clip.rs`) was written against tsMuxeR 2.7.0
(<https://github.com/justdan96/tsMuxer/releases>, Apache-2.0); the binary is no longer shipped.
To re-record `tests/fixtures/tsmuxer/*/{clpi.bin,mpls.bin,m2ts.json}` download the 2.7.0 release and run
`TSMUXER_RECORD=<path>/tsMuxeR.exe cargo test --test tsmuxer_native`; each case's `oracle.meta` is the
project file it was recorded from.
