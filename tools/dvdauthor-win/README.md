# Native dvdauthor/spumux for Windows

`bin/dvdauthor.exe` and `bin/spumux.exe` are built from the official sources
(<https://github.com/ldo/dvdauthor>, commit `fe8fe35`, 0.7.2 + fixes) with MSYS2 UCRT64 and are
statically linked: they only import Windows system DLLs, so no extra DLLs are needed in `bin/`.

## Rebuild

```sh
scoop install msys2
# in an MSYS2 UCRT64 shell:
pacman -S --needed git autoconf automake libtool make flex bison gettext-devel \
  mingw-w64-ucrt-x86_64-{gcc,pkgconf,libxml2,libpng,freetype,fribidi,libiconv,zlib,libdvdread}
bash tools/dvdauthor-win/build.sh
```

## Windows port

dvdauthor is POSIX code; the port needs no source changes, only shims passed at `make` time:

- `wincompat.h` (force-included): `mkdir(path, mode)`, `fsync`, `bzero`, and **binary mode for
  `open()`/`fopen()`/stdin/stdout**. Without it the CRT's text mode corrupts every MPEG stream.
- `include/langinfo.h`: `nl_langinfo(CODESET)` returns `UTF-8`.
- `include/netinet/in.h`: `htonl`/`htons`/`ntohl`/`ntohs`.

Only `dvdauthor` and `spumux` are built; `spuunmux`, `mpeg2desc` and `dvdunauthor` are not needed.

## License

dvdauthor is GPL-2.0 (`bin/dvdauthor-COPYING.txt`). When distributing the binaries, ship or offer
the corresponding source: the upstream commit above plus this directory.
