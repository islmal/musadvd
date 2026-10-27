/* Stub <netinet/in.h> for MinGW: spumux only needs the byte order helpers (x86 is little endian). */
#ifndef CX_NETINET_IN_H
#define CX_NETINET_IN_H

#include <stdint.h>

static inline uint32_t htonl(uint32_t x) { return __builtin_bswap32(x); }
static inline uint32_t ntohl(uint32_t x) { return __builtin_bswap32(x); }
static inline uint16_t htons(uint16_t x) { return __builtin_bswap16(x); }
static inline uint16_t ntohs(uint16_t x) { return __builtin_bswap16(x); }

#endif
