/* Compile-time prerequisite only: no encoder or runtime capability is implemented. */
#include "nvEncodeAPI.h"

_Static_assert(NVENCAPI_MAJOR_VERSION == 13, "Unexpected NVENC SDK major version");
_Static_assert(NVENCAPI_MINOR_VERSION == 0, "Unexpected NVENC SDK minor version");
_Static_assert(sizeof(GUID) == 16, "Unexpected GUID ABI");
