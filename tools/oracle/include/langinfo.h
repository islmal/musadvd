/* Stub <langinfo.h> for MinGW: dvdauthor only asks for the default character set. */
#ifndef CX_LANGINFO_H
#define CX_LANGINFO_H

#define CODESET 0

static inline const char *nl_langinfo(int item)
{
    (void)item;
    return "UTF-8";
}

#endif
