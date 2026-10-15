#!/usr/bin/env bash
# Builds native, statically linked Windows binaries of dvdauthor and spumux from the official
# sources (https://github.com/ldo/dvdauthor) and copies them into <project>/bin.
#
# Run in an MSYS2 UCRT64 shell:
#   MSYSTEM=UCRT64 bash -l tools/dvdauthor-win/build.sh [work-dir]
#
# Required MSYS2 packages:
#   git autoconf automake libtool make flex bison gettext-devel
#   mingw-w64-ucrt-x86_64-{gcc,pkgconf,libxml2,libpng,freetype,fribidi,libiconv,zlib,libdvdread}
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="$(cd "$HERE/../.." && pwd)"
WORK="${1:-$HERE/build}"
# Last upstream commit (0.7.2 + fixes, 2021-11-05).
REV="fe8fe3578f95f34889e7ed17591d02dceb4f42ed"

mkdir -p "$WORK"
cd "$WORK"
if [ ! -d dvdauthor ]; then
    git clone -q https://github.com/ldo/dvdauthor.git
fi
cd dvdauthor
git checkout -q "$REV"

[ -f configure ] || ./bootstrap
# configure's own feature tests must run without the shims.
./configure --prefix=/usr/local >configure.log 2>&1

cd src
rm -f ./*.o dvdauthor.exe spumux.exe .libs/*.exe

# The POSIX shims (wincompat.h, langinfo.h, netinet/in.h) and static library flags are passed
# at make time only.
CPPFLAGS="-include $HERE/wincompat.h -I$HERE/include -DLIBXML_STATIC -DFRIBIDI_LIB_STATIC"
XML2_LIBS="$(pkg-config --static --libs libxml-2.0) -liconv -lz -lbcrypt -lws2_32"
# A static freetype pulls in harfbuzz/graphite2, which are C++.
FREETYPE_LIBS="$(pkg-config --static --libs freetype2) -lz -lstdc++"
FRIBIDI_LIBS="$(pkg-config --static --libs fribidi)"
LIBPNG_LIBS="$(pkg-config --static --libs libpng)"

make CPPFLAGS="$CPPFLAGS" LDFLAGS="-all-static" \
    XML2_LIBS="$XML2_LIBS" FREETYPE_LIBS="$FREETYPE_LIBS" \
    FRIBIDI_LIBS="$FRIBIDI_LIBS" LIBPNG_LIBS="$LIBPNG_LIBS" \
    dvdauthor.exe spumux.exe >"$WORK/make.log" 2>&1 || {
    grep -E 'error|undefined reference' "$WORK/make.log" | sort -u | head -30
    exit 1
}

mkdir -p "$PROJECT/bin"
for exe in dvdauthor spumux; do
    # libtool puts the real binary into .libs/ when it exists.
    src="$exe.exe"
    [ -f ".libs/$exe.exe" ] && src=".libs/$exe.exe"
    strip -o "$PROJECT/bin/$exe.exe" "$src"
done
cp ../COPYING "$PROJECT/bin/dvdauthor-COPYING.txt"

echo "Built from ldo/dvdauthor@$REV:"
for exe in dvdauthor spumux; do
    printf '  %s  imports: ' "$PROJECT/bin/$exe.exe"
    objdump -p "$PROJECT/bin/$exe.exe" | awk '/DLL Name/ {printf "%s ", $3}'
    echo
done
