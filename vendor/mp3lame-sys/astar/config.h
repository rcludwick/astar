/* config.h for building LAME 3.100 with the `cc` crate instead of
 * autotools (astar patch; see ../VENDORED.md). Every value here is one
 * `configure` would detect on any C99 target astar builds for: Linux
 * (glibc and musl), macOS and Windows with clang or gcc. */
#ifndef ASTAR_LAME_CONFIG_H
#define ASTAR_LAME_CONFIG_H

#define STDC_HEADERS 1
#define HAVE_STDINT_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRING_H 1
#define HAVE_ERRNO_H 1
#define HAVE_FCNTL_H 1
#define HAVE_LIMITS_H 1
#define HAVE_STRCHR 1
#define HAVE_MEMCPY 1
#define PACKAGE "lame"
#define PROTOTYPES 1
#define USE_FAST_LOG 1
#define LAME_LIBRARY_BUILD 1

#include <stdint.h>
typedef float ieee754_float32_t;
typedef double ieee754_float64_t;
typedef long double ieee854_float80_t;

#endif
