/*
 * Minimal POSIX shims for building dvdauthor/spumux natively with MinGW-w64 (UCRT64).
 * Force-included into every translation unit via `-include wincompat.h`.
 */
#ifndef CX_WINCOMPAT_H
#define CX_WINCOMPAT_H
#ifdef _WIN32

#include <direct.h>
#include <fcntl.h>
#include <io.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* POSIX mkdir has a mode argument, the CRT one does not. */
#define mkdir(path, mode) _mkdir(path)
#define fsync(fd) _commit(fd)
#define bzero(p, n) memset((p), 0, (n))

/*
 * dvdauthor and spumux read and write MPEG data through open(), fopen() and stdin/stdout.
 * The Windows CRT defaults to text mode, which would translate CR/LF bytes and treat 0x1A as
 * EOF, corrupting every VOB. Switch everything to binary before main() runs.
 */
static void __attribute__((constructor)) cx_wincompat_binary_io(void)
{
    _set_fmode(_O_BINARY);
    _setmode(_fileno(stdin), _O_BINARY);
    _setmode(_fileno(stdout), _O_BINARY);
}

#endif /* _WIN32 */
#endif /* CX_WINCOMPAT_H */
