/*
 * Frame-owned NVENC bridge (FRAME-0104, corrected by FRAME-0104-R1, ADR 0006).
 *
 * Everything NVIDIA-specific lives here: the SDK header's structures, bitfields, GUIDs,
 * version macros and function table. Rust declares only the Frame-owned ABI in
 * nvenc_bridge.h, so no SDK layout is transcribed and no handwritten function table
 * exists. The bridge:
 *
 * - loads only the system-directory nvEncodeAPI64.dll, negotiates the driver's API
 *   version with NVIDIA's distinct max-version encoding and keeps the DLL loaded through
 *   a reference count until the last session is gone;
 * - queries the driver's real capabilities and input formats before it accepts the
 *   requested configuration, so an unsupported capability is explicit and never a
 *   fallback to software, another adapter or another provider;
 * - applies codec-specific progressive configurations, with no
 *   B-frames, no lookahead, `enablePTD = 1`, `frameIntervalP = 1`,
 *   `enableEncodeAsync = 1`, `enableOutputInVidmem = 0`, auto level, bounded GOP and
 *   repeated SPS/PPS at IDRs. The caller's policy chooses the preset/tuning pair
 *   or independently qualified P1..P7/tuning overrides, the rate-control mode
 *   (CBR, VBR or CQP) and bitrate/QP. Capabilities and sequence headers are queried;
 *   configuration fields are snapshots accepted by initialization, not readback from
 *   a get-current-configuration API. A pair or mode the driver
 *   does not offer is an explicit failure rather than a substitution;
 * - registers capture textures as DirectX input images, maps one for each submission and
 *   keeps it mapped until that submission's bitstream has been locked and copied. A
 *   picture accepted with `NV_ENC_ERR_NEED_MORE_INPUT` is a **buffered** acceptance: the
 *   input stays mapped and the output slot and its event stay owned until that picture's
 *   access unit arrives, with later input or with end of stream.
 *   `NV_ENC_ERR_ENCODER_BUSY` is different: nothing was accepted, nothing was buffered
 *   and the input is unmapped for an unchanged retry;
 * - locks a bitstream with `doNotWait = 0` only after that output's completion event has
 *   signalled, as NVIDIA's threading guidance requires;
 * - bounds every buffer: output buffers, registrations, detail text and initialization
 *   bytes all have fixed capacities, and no bitstream is ever truncated, partially copied
 *   or silently accepted;
 * - records the exact NVENC call it is inside (name, start time, thread, running flag) so
 *   an unfinished call is named from evidence instead of inferred from counters, and
 *   never records pixel data.
 *
 * Every entry point returns an int32_t Frame-owned category and fills a bounded outcome
 * with the native NVENCSTATUS and the failing API. Nothing here throws, longjmps or
 * leaves a partially initialized session behind.
 */

#include "nvenc_bridge.h"

#include "nvEncodeAPI.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* The native bitstream capacity for one output buffer: the accepted 8 MiB access-unit
 * bound plus one page of headroom, so a driver-truncated unit is distinguishable from a
 * genuinely maximal one. Declared first because the probe and the session report it. */
static uint32_t frame_nvenc_output_capacity(void) {
    return (uint32_t)FRAME_NVENC_MAX_ACCESS_UNIT_BYTES +
           (uint32_t)FRAME_NVENC_ACCESS_UNIT_HEADROOM_BYTES;
}

/* Declared in the Frame-owned header; forward-declared here because the probe and the
 * session open path release resources before their own definitions appear. */
int32_t frame_nvenc_session_close(FrameNvencSession *session, FrameNvencOutcome *out_outcome);

/* Preparation and qualification have no returned session owner. Keep their stack,
 * device and DLL alive until cleanup is confirmed; a native hang may block this caller. */
static int32_t frame_nvenc_close_owned(FrameNvencSession *session, FrameNvencOutcome *outcome) {
    FrameNvencOutcome first, current;
    int32_t result = frame_nvenc_session_close(session, &first);
    int32_t initial = result;
    while (result == FRAME_NVENC_CLEANUP_UNSAFE) {
        Sleep(100u);
        result = frame_nvenc_session_close(session, &current);
    }
    if (outcome != NULL) {
        *outcome = first;
        if (initial == FRAME_NVENC_CLEANUP_UNSAFE) {
            outcome->category = FRAME_NVENC_FAILED;
            snprintf(outcome->detail, sizeof(outcome->detail), "Cleanup initially failed with status %d; owner-thread retries later confirmed destruction. Initial failure: %.300s", first.status, first.detail);
        }
    }
    return initial == FRAME_NVENC_CLEANUP_UNSAFE ? FRAME_NVENC_FAILED : initial;
}

/* ------------------------------------------------------------------ outcomes */

static void frame_nvenc_clear(FrameNvencOutcome *outcome) {
    if (outcome == NULL) {
        return;
    }
    outcome->category = FRAME_NVENC_OK;
    outcome->status = -1;
    outcome->detail[0] = '\0';
}

static int32_t frame_nvenc_fail(FrameNvencOutcome *outcome, int32_t category, int32_t status,
                                const char *format, ...) {
    if (outcome != NULL) {
        va_list arguments;
        outcome->category = category;
        outcome->status = status;
        va_start(arguments, format);
        (void)vsnprintf(outcome->detail, (size_t)FRAME_NVENC_DETAIL_BYTES, format, arguments);
        va_end(arguments);
        outcome->detail[FRAME_NVENC_DETAIL_BYTES - 1u] = '\0';
    }
    return category;
}

static int32_t frame_nvenc_ok(FrameNvencOutcome *outcome, const char *format, ...) {
    if (outcome != NULL) {
        va_list arguments;
        outcome->category = FRAME_NVENC_OK;
        outcome->status = (int32_t)NV_ENC_SUCCESS;
        va_start(arguments, format);
        (void)vsnprintf(outcome->detail, (size_t)FRAME_NVENC_DETAIL_BYTES, format, arguments);
        va_end(arguments);
        outcome->detail[FRAME_NVENC_DETAIL_BYTES - 1u] = '\0';
    }
    return FRAME_NVENC_OK;
}

/* Only documented capability and session conditions are classified; everything else stays
 * a failure, so a defect can never be reported as absent hardware. */
static int32_t frame_nvenc_category(NVENCSTATUS status) {
    switch (status) {
        case NV_ENC_SUCCESS:
            return FRAME_NVENC_OK;
        case NV_ENC_ERR_NO_ENCODE_DEVICE:
        case NV_ENC_ERR_UNSUPPORTED_DEVICE:
        case NV_ENC_ERR_INVALID_DEVICE:
        case NV_ENC_ERR_INVALID_ENCODERDEVICE:
            return FRAME_NVENC_UNAVAILABLE;
        case NV_ENC_ERR_INVALID_VERSION:
            return FRAME_NVENC_VERSION;
        case NV_ENC_ERR_UNSUPPORTED_PARAM:
        case NV_ENC_ERR_UNIMPLEMENTED:
        case NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY:
            return FRAME_NVENC_UNSUPPORTED;
        case NV_ENC_ERR_INVALID_PTR:
        case NV_ENC_ERR_INVALID_PARAM:
        case NV_ENC_ERR_INVALID_CALL:
        case NV_ENC_ERR_INVALID_EVENT:
        case NV_ENC_ERR_ENCODER_NOT_INITIALIZED:
        case NV_ENC_ERR_RESOURCE_NOT_REGISTERED:
        case NV_ENC_ERR_RESOURCE_NOT_MAPPED:
        case NV_ENC_ERR_NOT_ENOUGH_BUFFER:
            return FRAME_NVENC_INVALID;
        case NV_ENC_ERR_NEED_MORE_INPUT:
        case NV_ENC_ERR_ENCODER_BUSY:
        case NV_ENC_ERR_LOCK_BUSY:
            return FRAME_NVENC_BUSY;
        default:
            return FRAME_NVENC_FAILED;
    }
}

/* Formats a native failure with the API name, the arguments and the raw status. */
static int32_t frame_nvenc_native_failure(FrameNvencOutcome *outcome, const char *api,
                                          NVENCSTATUS status, const char *context) {
    return frame_nvenc_fail(outcome, frame_nvenc_category(status), (int32_t)status,
                            "%s failed with NVENCSTATUS %d (%s).", api, (int32_t)status, context);
}

/* ------------------------------------------------------------------- library */

struct FrameNvencLibrary {
    HMODULE module;
    NV_ENCODE_API_FUNCTION_LIST functions;
    uint32_t driver_api_version;
    uint32_t driver_major;
    uint32_t driver_minor;
    /* Nonzero once the DLL and the whole function table resolved. */
    int32_t usable;
    /* Retained load failure, so every later acquire reports the same reason. */
    int32_t failure_category;
    int32_t failure_status;
    char failure_detail[FRAME_NVENC_DETAIL_BYTES];
};

static SRWLOCK frame_nvenc_lock = SRWLOCK_INIT;
static FrameNvencLibrary frame_nvenc_library;
static uint32_t frame_nvenc_references;

/* The SDK's function list is only usable when every entry Frame calls resolved. */
static const char *frame_nvenc_missing_entry(void) {
    const NV_ENCODE_API_FUNCTION_LIST *functions = &frame_nvenc_library.functions;
    if (functions->nvEncOpenEncodeSessionEx == NULL) {
        return "nvEncOpenEncodeSessionEx";
    }
    if (functions->nvEncGetEncodeCaps == NULL) {
        return "nvEncGetEncodeCaps";
    }
    if (functions->nvEncGetInputFormats == NULL) {
        return "nvEncGetInputFormats";
    }
    if (functions->nvEncGetEncodeGUIDs == NULL || functions->nvEncGetEncodeGUIDCount == NULL ||
        functions->nvEncGetEncodeProfileGUIDs == NULL || functions->nvEncGetEncodeProfileGUIDCount == NULL ||
        functions->nvEncGetInputFormatCount == NULL) {
        return "codec/profile/input-format enumeration";
    }
    if (functions->nvEncGetEncodePresetConfigEx == NULL) {
        return "nvEncGetEncodePresetConfigEx";
    }
    if (functions->nvEncInitializeEncoder == NULL) {
        return "nvEncInitializeEncoder";
    }
    if (functions->nvEncGetSequenceParams == NULL) {
        return "nvEncGetSequenceParams";
    }
    if (functions->nvEncRegisterAsyncEvent == NULL) {
        return "nvEncRegisterAsyncEvent";
    }
    if (functions->nvEncUnregisterAsyncEvent == NULL) {
        return "nvEncUnregisterAsyncEvent";
    }
    if (functions->nvEncCreateBitstreamBuffer == NULL) {
        return "nvEncCreateBitstreamBuffer";
    }
    if (functions->nvEncDestroyBitstreamBuffer == NULL) {
        return "nvEncDestroyBitstreamBuffer";
    }
    if (functions->nvEncRegisterResource == NULL) {
        return "nvEncRegisterResource";
    }
    if (functions->nvEncUnregisterResource == NULL) {
        return "nvEncUnregisterResource";
    }
    if (functions->nvEncMapInputResource == NULL) {
        return "nvEncMapInputResource";
    }
    if (functions->nvEncUnmapInputResource == NULL) {
        return "nvEncUnmapInputResource";
    }
    if (functions->nvEncEncodePicture == NULL) {
        return "nvEncEncodePicture";
    }
    if (functions->nvEncLockBitstream == NULL) {
        return "nvEncLockBitstream";
    }
    if (functions->nvEncUnlockBitstream == NULL) {
        return "nvEncUnlockBitstream";
    }
    if (functions->nvEncDestroyEncoder == NULL) {
        return "nvEncDestroyEncoder";
    }
    if (functions->nvEncGetLastErrorString == NULL) {
        return "nvEncGetLastErrorString";
    }
    return NULL;
}

static void frame_nvenc_record_failure(int32_t category, int32_t status, const char *detail) {
    frame_nvenc_library.failure_category = category;
    frame_nvenc_library.failure_status = status;
    (void)snprintf(frame_nvenc_library.failure_detail, (size_t)FRAME_NVENC_DETAIL_BYTES, "%s",
                   detail);
    frame_nvenc_library.failure_detail[FRAME_NVENC_DETAIL_BYTES - 1u] = '\0';
}

/* Loads the DLL and resolves the API. Called with the lock held. */
static int32_t frame_nvenc_load_locked(void) {
    wchar_t path[MAX_PATH];
    UINT length;
    HMODULE module;
    FARPROC get_version_entry;
    FARPROC create_instance_entry;
    NVENCSTATUS status;
    uint32_t driver_version = 0;
    const char *missing;

    if (frame_nvenc_library.usable != 0) {
        return FRAME_NVENC_OK;
    }
    if (frame_nvenc_library.failure_category != FRAME_NVENC_OK) {
        return frame_nvenc_library.failure_category;
    }

    /* The system directory only: never the working directory, PATH or the application
     * directory, so a planted DLL cannot be loaded. */
    length = GetSystemDirectoryW(path, (UINT)(sizeof(path) / sizeof(path[0])));
    if (length == 0u || length >= (UINT)(sizeof(path) / sizeof(path[0]))) {
        frame_nvenc_record_failure(
            FRAME_NVENC_UNAVAILABLE, -1,
            "GetSystemDirectoryW did not return the Windows system directory, so the installed "
            "NVENC library cannot be located.");
        return FRAME_NVENC_UNAVAILABLE;
    }
    {
        static const wchar_t suffix[] = L"\\nvEncodeAPI64.dll";
        const size_t suffix_length = sizeof(suffix) / sizeof(suffix[0]);
        if ((size_t)length + suffix_length > (sizeof(path) / sizeof(path[0]))) {
            frame_nvenc_record_failure(FRAME_NVENC_UNAVAILABLE, -1,
                                       "The Windows system directory path is longer than the "
                                       "bounded NVENC library path buffer.");
            return FRAME_NVENC_UNAVAILABLE;
        }
        (void)memcpy(path + length, suffix, suffix_length * sizeof(wchar_t));
    }

    module = LoadLibraryW(path);
    if (module == NULL) {
        char detail[FRAME_NVENC_DETAIL_BYTES];
        (void)snprintf(detail, sizeof(detail),
                       "The system NVENC library nvEncodeAPI64.dll is not loadable (Win32 error "
                       "%lu), so no NVIDIA hardware encoder path is available and this build has "
                       "no software or other-provider fallback.",
                       (unsigned long)GetLastError());
        frame_nvenc_record_failure(FRAME_NVENC_UNAVAILABLE, -1, detail);
        return FRAME_NVENC_UNAVAILABLE;
    }

    get_version_entry = GetProcAddress(module, "NvEncodeAPIGetMaxSupportedVersion");
    create_instance_entry = GetProcAddress(module, "NvEncodeAPICreateInstance");
    if (get_version_entry == NULL || create_instance_entry == NULL) {
        (void)FreeLibrary(module);
        frame_nvenc_record_failure(
            FRAME_NVENC_UNAVAILABLE, -1,
            "nvEncodeAPI64.dll does not export both NvEncodeAPIGetMaxSupportedVersion and "
            "NvEncodeAPICreateInstance.");
        return FRAME_NVENC_UNAVAILABLE;
    }

    /* The declared entry points are the SDK's own __stdcall signatures. */
    status = ((NVENCSTATUS(NVENCAPI *)(uint32_t *))get_version_entry)(&driver_version);
    if (status != NV_ENC_SUCCESS || driver_version == 0u) {
        (void)FreeLibrary(module);
        frame_nvenc_record_failure(
            FRAME_NVENC_UNAVAILABLE, (int32_t)status,
            "NvEncodeAPIGetMaxSupportedVersion did not report a usable NVENC API version.");
        return FRAME_NVENC_UNAVAILABLE;
    }

    /* NVIDIA encodes this answer as major in the high bits and minor in the low four. */
    frame_nvenc_library.driver_api_version = driver_version;
    frame_nvenc_library.driver_major = driver_version >> 4;
    frame_nvenc_library.driver_minor = driver_version & 0xFu;
    if (frame_nvenc_library.driver_major < (uint32_t)NVENCAPI_MAJOR_VERSION ||
        (frame_nvenc_library.driver_major == (uint32_t)NVENCAPI_MAJOR_VERSION &&
         frame_nvenc_library.driver_minor < (uint32_t)NVENCAPI_MINOR_VERSION)) {
        char detail[FRAME_NVENC_DETAIL_BYTES];
        (void)snprintf(detail, sizeof(detail),
                       "The installed NVIDIA driver supports NVENC API %u.%u, but this bridge was "
                       "compiled against API %u.%u, so its structures must not be used.",
                       frame_nvenc_library.driver_major, frame_nvenc_library.driver_minor,
                       (uint32_t)NVENCAPI_MAJOR_VERSION, (uint32_t)NVENCAPI_MINOR_VERSION);
        (void)FreeLibrary(module);
        frame_nvenc_record_failure(FRAME_NVENC_VERSION, (int32_t)status, detail);
        return FRAME_NVENC_VERSION;
    }

    (void)memset(&frame_nvenc_library.functions, 0, sizeof(frame_nvenc_library.functions));
    frame_nvenc_library.functions.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    status = ((NVENCSTATUS(NVENCAPI *)(NV_ENCODE_API_FUNCTION_LIST *))create_instance_entry)(
        &frame_nvenc_library.functions);
    if (status != NV_ENC_SUCCESS) {
        (void)FreeLibrary(module);
        frame_nvenc_record_failure(FRAME_NVENC_FAILED, (int32_t)status,
                                   "NvEncodeAPICreateInstance failed, so the NVENC function table "
                                   "could not be resolved.");
        return FRAME_NVENC_FAILED;
    }

    missing = frame_nvenc_missing_entry();
    if (missing != NULL) {
        char detail[FRAME_NVENC_DETAIL_BYTES];
        (void)snprintf(detail, sizeof(detail),
                       "The NVENC function table is missing %s, so this driver cannot serve the "
                       "asynchronous Direct3D encoder path.",
                       missing);
        (void)FreeLibrary(module);
        frame_nvenc_record_failure(FRAME_NVENC_UNAVAILABLE, -1, detail);
        return FRAME_NVENC_UNAVAILABLE;
    }

    frame_nvenc_library.module = module;
    frame_nvenc_library.usable = 1;
    return FRAME_NVENC_OK;
}

int32_t frame_nvenc_library_acquire(FrameNvencLibrary **out_library,
                                    FrameNvencLibraryInfo *out_info,
                                    FrameNvencOutcome *out_outcome) {
    int32_t result;
    frame_nvenc_clear(out_outcome);
    if (out_library == NULL || out_info == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "frame_nvenc_library_acquire needs non-null out-parameters.");
    }
    *out_library = NULL;

    AcquireSRWLockExclusive(&frame_nvenc_lock);
    result = frame_nvenc_load_locked();
    if (result == FRAME_NVENC_OK) {
        frame_nvenc_references += 1u;
        *out_library = &frame_nvenc_library;
        out_info->api_version_max = frame_nvenc_library.driver_api_version;
        out_info->driver_major = frame_nvenc_library.driver_major;
        out_info->driver_minor = frame_nvenc_library.driver_minor;
        out_info->api_version_required = (uint32_t)NVENCAPI_VERSION;
        (void)frame_nvenc_ok(out_outcome,
                             "nvEncodeAPI64.dll loaded from the Windows system directory; driver "
                             "API %u.%u, bridge API %u.%u.",
                             frame_nvenc_library.driver_major, frame_nvenc_library.driver_minor,
                             (uint32_t)NVENCAPI_MAJOR_VERSION, (uint32_t)NVENCAPI_MINOR_VERSION);
    } else {
        (void)frame_nvenc_fail(out_outcome, result, frame_nvenc_library.failure_status, "%s",
                               frame_nvenc_library.failure_detail);
    }
    ReleaseSRWLockExclusive(&frame_nvenc_lock);
    return result;
}

void frame_nvenc_library_release(FrameNvencLibrary *library) {
    if (library == NULL) {
        return;
    }
    AcquireSRWLockExclusive(&frame_nvenc_lock);
    if (library == &frame_nvenc_library) {
        if (frame_nvenc_references > 0u) {
            frame_nvenc_references -= 1u;
        }
        if (frame_nvenc_references == 0u && frame_nvenc_library.module != NULL) {
            /* No session or worker can still reference the DLL once the last handle is
             * released, because every session holds one. */
            (void)FreeLibrary(frame_nvenc_library.module);
            (void)memset(&frame_nvenc_library, 0, sizeof(frame_nvenc_library));
        }
    }
    ReleaseSRWLockExclusive(&frame_nvenc_lock);
}

/* ---------------------------------------------------------------- session */

struct FrameNvencSession {
    FrameNvencLibrary *library;
    void *encoder;
    int32_t initialized;
    FrameNvencSettings settings;
    GUID codec_guid;
    NV_ENC_BUFFER_FORMAT input_format;
    MASTERING_DISPLAY_INFO mastering;
    CONTENT_LIGHT_LEVEL content_light;
    uint32_t mastering_present;
    uint32_t content_light_present;
    uint32_t codec;
    void *outputs[FRAME_NVENC_OUTPUT_BUFFERS];
    void *events[FRAME_NVENC_OUTPUT_BUFFERS];
    int32_t event_registered[FRAME_NVENC_OUTPUT_BUFFERS];
    int32_t output_in_use[FRAME_NVENC_OUTPUT_BUFFERS];
    int32_t output_locked[FRAME_NVENC_OUTPUT_BUFFERS];
    int32_t output_complete[FRAME_NVENC_OUTPUT_BUFFERS];
    /* The registration each owned output buffer belongs to, so completion unmaps exactly
     * that mapping and never another submission's. */
    uint32_t output_registration[FRAME_NVENC_OUTPUT_BUFFERS];
    struct {
        void *registered;
        void *texture;
        uint32_t width;
        uint32_t height;
        int32_t used;
        NV_ENC_INPUT_PTR mapped;
    } inputs[FRAME_NVENC_INPUT_REGISTRATIONS];
    void *eos_event;
    int32_t eos_registered;
    int32_t eos_submitted;
    uint64_t witness_id;
    uint64_t submitted_frames;
    uint32_t native_calls;
    uint32_t busy_results;
    uint32_t pending_inputs;
    uint32_t buffered_pictures;
};

/* NVENC and event APIs this bridge can be inside, so an unfinished call is named exactly. */
static const char *const frame_nvenc_api_names[] = {
    "idle",
    "NvEncOpenEncodeSessionEx",
    "NvEncGetEncodeCaps",
    "NvEncGetInputFormats",
    "NvEncGetEncodePresetConfigEx",
    "NvEncInitializeEncoder",
    "NvEncGetSequenceParams",
    "NvEncCreateBitstreamBuffer",
    "NvEncDestroyBitstreamBuffer",
    "NvEncRegisterAsyncEvent",
    "NvEncUnregisterAsyncEvent",
    "NvEncRegisterResource",
    "NvEncUnregisterResource",
    "NvEncMapInputResource",
    "NvEncUnmapInputResource",
    "NvEncEncodePicture",
    "NvEncLockBitstream",
    "NvEncUnlockBitstream",
    "NvEncDestroyEncoder",
    "WaitForSingleObject(completion event)",
    "WaitForSingleObject(EOS event)",
    "ResetEvent(completion event)",
};

enum {
    FRAME_NVENC_API_IDLE = 0,
    FRAME_NVENC_API_OPEN = 1,
    FRAME_NVENC_API_CAPS = 2,
    FRAME_NVENC_API_FORMATS = 3,
    FRAME_NVENC_API_PRESET = 4,
    FRAME_NVENC_API_INITIALIZE = 5,
    FRAME_NVENC_API_SEQUENCE = 6,
    FRAME_NVENC_API_CREATE_BITSTREAM = 7,
    FRAME_NVENC_API_DESTROY_BITSTREAM = 8,
    FRAME_NVENC_API_REGISTER_EVENT = 9,
    FRAME_NVENC_API_UNREGISTER_EVENT = 10,
    FRAME_NVENC_API_REGISTER_RESOURCE = 11,
    FRAME_NVENC_API_UNREGISTER_RESOURCE = 12,
    FRAME_NVENC_API_MAP = 13,
    FRAME_NVENC_API_UNMAP = 14,
    FRAME_NVENC_API_ENCODE = 15,
    FRAME_NVENC_API_LOCK = 16,
    FRAME_NVENC_API_UNLOCK = 17,
    FRAME_NVENC_API_DESTROY = 18,
    FRAME_NVENC_API_WAIT_EVENT = 19,
    FRAME_NVENC_API_WAIT_EOS = 20,
    FRAME_NVENC_API_RESET_EVENT = 21
};

/* Slots are reserved/released by one owning session; observers copy values under
 * a short shared lock. They never follow a session pointer and never block on a
 * native call. Generation identity prevents a reused slot looking like its predecessor. */
#define FRAME_WITNESS_SLOTS 64u
static SRWLOCK frame_witness_lock = SRWLOCK_INIT;
static FrameNvencPhase frame_witness_records[FRAME_WITNESS_SLOTS];
static uint64_t frame_witness_sequence = 0u;
static uint64_t frame_witness_qpc(void) {
    LARGE_INTEGER value;
    return QueryPerformanceCounter(&value) ? (uint64_t)value.QuadPart : 0u;
}
uint64_t frame_witness_create(const char *initial_phase) {
    uint32_t i;
    uint64_t id = 0u;
    LARGE_INTEGER frequency;
    AcquireSRWLockExclusive(&frame_witness_lock);
    if (frame_witness_sequence < (UINT64_MAX >> 6)) {
        for (i = 0u; i < FRAME_WITNESS_SLOTS; ++i) {
            FrameNvencPhase *record = &frame_witness_records[i];
            if (record->session_id == 0u) {
                memset(record, 0, sizeof(*record));
                id = (++frame_witness_sequence << 6) | i;
                record->session_id = id;
                record->ticks_per_second = QueryPerformanceFrequency(&frequency) ? (uint64_t)frequency.QuadPart : 0u;
                snprintf(record->api, sizeof(record->api), "%s", initial_phase);
                break;
            }
        }
    }
    ReleaseSRWLockExclusive(&frame_witness_lock);
    return id;
}
void frame_witness_release(uint64_t id) {
    FrameNvencPhase *record = &frame_witness_records[id & 63u];
    AcquireSRWLockExclusive(&frame_witness_lock);
    if (id != 0u && record->session_id == id) memset(record, 0, sizeof(*record));
    ReleaseSRWLockExclusive(&frame_witness_lock);
}
void frame_witness_begin(uint64_t id, const char *api) {
    FrameNvencPhase *record = &frame_witness_records[id & 63u];
    AcquireSRWLockExclusive(&frame_witness_lock);
    if (id != 0u && record->session_id == id) {
        snprintf(record->api, sizeof(record->api), "%s", api);
        record->thread_id = GetCurrentThreadId();
        record->started_qpc = frame_witness_qpc();
        record->ended_qpc = 0u;
        record->running = 1u;
    }
    ReleaseSRWLockExclusive(&frame_witness_lock);
}
void frame_witness_end(uint64_t id) {
    FrameNvencPhase *record = &frame_witness_records[id & 63u];
    AcquireSRWLockExclusive(&frame_witness_lock);
    if (id != 0u && record->session_id == id) {
        record->ended_qpc = frame_witness_qpc();
        record->running = 0u;
        record->completed_calls += 1u;
    }
    ReleaseSRWLockExclusive(&frame_witness_lock);
}
uint32_t frame_witness_snapshots(FrameNvencPhase *records, uint32_t capacity) {
    uint32_t i, count = 0u;
    if (!TryAcquireSRWLockShared(&frame_witness_lock)) return UINT32_MAX;
    for (i = 0u; i < FRAME_WITNESS_SLOTS && count < capacity; ++i) {
        if (frame_witness_records[i].session_id != 0u) {
            records[count] = frame_witness_records[i];
            records[count].observed_qpc = frame_witness_qpc();
            ++count;
        }
    }
    ReleaseSRWLockShared(&frame_witness_lock);
    return count;
}
static void frame_nvenc_phase_begin(FrameNvencSession *session, uint32_t api) {
    if (session != NULL) frame_witness_begin(session->witness_id, frame_nvenc_api_names[api]);
}
static void frame_nvenc_phase_end(FrameNvencSession *session) {
    if (session != NULL) frame_witness_end(session->witness_id);
}
void frame_nvenc_session_phase(FrameNvencSession *session, FrameNvencPhase *out_phase) {
    memset(out_phase, 0, sizeof(*out_phase));
    snprintf(out_phase->api, sizeof(out_phase->api), "unknown/closed-or-contended");
    if (session != NULL && TryAcquireSRWLockShared(&frame_witness_lock)) {
        FrameNvencPhase *record = &frame_witness_records[session->witness_id & 63u];
        if (record->session_id == session->witness_id && record->session_id != 0u) {
            *out_phase = *record;
            out_phase->observed_qpc = frame_witness_qpc();
        }
        ReleaseSRWLockShared(&frame_witness_lock);
    }
}

static int32_t frame_nvenc_capability(FrameNvencSession *session, NV_ENC_CAPS capability,
                                      int32_t *out_value, FrameNvencOutcome *out_outcome) {
    NV_ENC_CAPS_PARAM parameter;
    NVENCSTATUS status;
    int32_t value = 0;

    (void)memset(&parameter, 0, sizeof(parameter));
    parameter.version = NV_ENC_CAPS_PARAM_VER;
    parameter.capsToQuery = capability;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_CAPS);
    status = session->library->functions.nvEncGetEncodeCaps(session->encoder,
                                                            session->codec_guid, &parameter,
                                                            &value);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        return frame_nvenc_native_failure(out_outcome, "NvEncGetEncodeCaps", status,
                                          "the selected codec capability query failed");
    }
    *out_value = value;
    return FRAME_NVENC_OK;
}

/*
 * Queries capabilities and input formats, obtains the requested preset/tuning pair and
 * overrides every field the accepted configuration requires. Reports exactly what was
 * applied.
 */
static int32_t frame_nvenc_configure(FrameNvencSession *session, uint32_t width, uint32_t height,
                                     uint32_t rate_num, uint32_t rate_den, uint32_t bitrate_bps,
                                     uint32_t max_bitrate_bps, uint32_t gop_length,
                                     uint32_t rate_control_mode, uint32_t preset, uint32_t codec,
                                     const FrameNvencSessionRequest *request,
                                     FrameNvencOutcome *out_outcome) {
    NVENCSTATUS status;
    NV_ENC_PRESET_CONFIG preset_config;
    NV_ENC_INITIALIZE_PARAMS initialize;
    NV_ENC_CONFIG config;
    NV_ENC_SEQUENCE_PARAM_PAYLOAD payload;
    NV_ENC_BUFFER_FORMAT formats[16];
    const GUID *preset_guid;
    NV_ENC_TUNING_INFO tuning;
    const char *preset_name;
    uint32_t format_count = 0u;
    uint32_t index;
    uint8_t sequence[FRAME_NVENC_CODEC_CONFIG_BYTES];
    uint32_t sequence_bytes = 0u;
    int32_t caps_async = 0;
    int32_t caps_width = 0;
    int32_t caps_height = 0;
    int32_t caps_rc = 0;
    int32_t input_argb = 0;
    int32_t caps_10bit = 0;
    int32_t caps_b_frames = 0, caps_lookahead = 0, caps_temporal_aq = 0;
    uint32_t input_formats = 0u;
    GUID guids[64];
    uint32_t guid_count = 0u;
    uint32_t written = 0u;
    int32_t found = 0;
    GUID profile;
    int32_t av1 = codec == FRAME_NVENC_CODEC_AV1 || codec == FRAME_NVENC_CODEC_AV1_MAIN10;
    int32_t main10 = codec == FRAME_NVENC_CODEC_HEVC_MAIN10 || codec == FRAME_NVENC_CODEC_AV1_MAIN10;
    int32_t lossless = preset == FRAME_NVENC_PRESET_LOSSLESS;
    if (lossless && (codec != FRAME_NVENC_CODEC_H264 || request->input_format != FRAME_NVENC_INPUT_RGB ||
                     rate_control_mode != FRAME_NVENC_RATE_CONTROL_LOSSLESS)) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "Lossless requires native H.264 RGB input and lossless rate control.");
    }

    if (codec != FRAME_NVENC_CODEC_H264 && codec != FRAME_NVENC_CODEC_HEVC_MAIN10 && !av1) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "The requested codec %u is not supported by this bridge.", codec);
    }
    session->codec = codec;
    session->codec_guid = av1 ? NV_ENC_CODEC_AV1_GUID : (codec == FRAME_NVENC_CODEC_HEVC_MAIN10
                              ? NV_ENC_CODEC_HEVC_GUID : NV_ENC_CODEC_H264_GUID);
    session->input_format = main10
                                ? NV_ENC_BUFFER_FORMAT_ABGR10 : NV_ENC_BUFFER_FORMAT_ARGB;
    if (request->input_format == FRAME_NVENC_INPUT_NV12 && !main10) {
        session->input_format = NV_ENC_BUFFER_FORMAT_NV12;
    } else if (request->input_format == FRAME_NVENC_INPUT_P010 && main10) {
        session->input_format = NV_ENC_BUFFER_FORMAT_YUV420_10BIT;
    } else if (request->input_format != FRAME_NVENC_INPUT_RGB) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "The input bit depth does not match the requested codec profile.");
    }
    profile = av1 ? NV_ENC_AV1_PROFILE_MAIN_GUID : (main10
        ? NV_ENC_HEVC_PROFILE_MAIN10_GUID : NV_ENC_H264_PROFILE_HIGH_GUID);
    if (lossless) profile = NV_ENC_H264_PROFILE_HIGH_444_GUID;
    /* Enumerate the actual codec/profile before initialization; no vendor heuristics. */
    status = session->library->functions.nvEncGetEncodeGUIDCount(session->encoder, &guid_count);
    if (status != NV_ENC_SUCCESS || guid_count > 64u) return frame_nvenc_fail(
        out_outcome, FRAME_NVENC_UNSUPPORTED, (int32_t)status, "Codec enumeration failed or exceeded its bound.");
    status = session->library->functions.nvEncGetEncodeGUIDs(session->encoder, guids, 64u, &written);
    if (status != NV_ENC_SUCCESS || written > 64u) return frame_nvenc_fail(
        out_outcome, FRAME_NVENC_UNSUPPORTED, (int32_t)status, "Codec GUID enumeration failed.");
    for (index = 0u; index < written; ++index) if (memcmp(&guids[index], &session->codec_guid, sizeof(GUID)) == 0) found = 1;
    if (!found) return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1, "The hardware does not advertise this codec.");
    status = session->library->functions.nvEncGetEncodeProfileGUIDCount(session->encoder, session->codec_guid, &guid_count);
    if (status != NV_ENC_SUCCESS || guid_count > 64u) return frame_nvenc_fail(
        out_outcome, FRAME_NVENC_UNSUPPORTED, (int32_t)status, "Profile enumeration failed or exceeded its bound.");
    status = session->library->functions.nvEncGetEncodeProfileGUIDs(session->encoder, session->codec_guid, guids, 64u, &written);
    if (status != NV_ENC_SUCCESS || written > 64u) return frame_nvenc_fail(
        out_outcome, FRAME_NVENC_UNSUPPORTED, (int32_t)status, "Profile GUID enumeration failed.");
    found = 0;
    for (index = 0u; index < written; ++index) if (memcmp(&guids[index], &profile, sizeof(GUID)) == 0) found = 1;
    if (!found) return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1, "The hardware does not advertise this encoder profile.");

    if (width == 0u || height == 0u || rate_num == 0u || rate_den == 0u || (bitrate_bps == 0u && !lossless && rate_control_mode != FRAME_NVENC_RATE_CONTROL_CQP) ||
        max_bitrate_bps < bitrate_bps || gop_length == 0u) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "The requested encoder configuration must have positive "
                                "dimensions, rate, bitrate and GOP length, and a peak bitrate "
                                "that is not below the average bitrate.");
    }
    if (lossless) {
        int32_t supported = 0;
        if (frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE, &supported,
                                  out_outcome) != FRAME_NVENC_OK) return out_outcome->category;
        if (supported != 1) return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
            "The hardware does not support lossless encoding.");
        if (frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE, &supported,
                                  out_outcome) != FRAME_NVENC_OK) return out_outcome->category;
        if (supported != 1) return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
            "The hardware does not support YUV 4:4:4 encoding.");
        preset_guid = &NV_ENC_PRESET_P1_GUID;
        tuning = NV_ENC_TUNING_INFO_LOSSLESS;
        preset_name = "P1 / lossless";
    } else if (preset == FRAME_NVENC_PRESET_HIGH_QUALITY) {
        preset_guid = &NV_ENC_PRESET_P5_GUID;
        tuning = NV_ENC_TUNING_INFO_HIGH_QUALITY;
        preset_name = "P5 / high quality";
    } else if (preset == FRAME_NVENC_PRESET_MAXIMUM_QUALITY) {
        preset_guid = &NV_ENC_PRESET_P7_GUID;
        tuning = NV_ENC_TUNING_INFO_HIGH_QUALITY;
        preset_name = "P7 / high quality";
    } else if (preset == FRAME_NVENC_PRESET_LOW_LATENCY) {
        preset_guid = &NV_ENC_PRESET_P1_GUID;
        tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        preset_name = "P1 / ultra-low-latency";
    } else {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "The requested encoder preset %u is outside the bounded set "
                                "this bridge applies.",
                                preset);
    }
    if (rate_control_mode != FRAME_NVENC_RATE_CONTROL_CBR &&
        rate_control_mode != FRAME_NVENC_RATE_CONTROL_VBR && rate_control_mode != FRAME_NVENC_RATE_CONTROL_CQP && !lossless) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "The requested rate-control mode %u is outside the bounded set "
                                "this bridge applies.",
                                rate_control_mode);
    }
    /* 4:2:0 chroma cannot represent odd dimensions. */
    if ((width % 2u) != 0u || (height % 2u) != 0u) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "A %ux%u request cannot be encoded as 4:2:0, which needs "
                                "even dimensions.",
                                width, height);
    }

    if (frame_nvenc_capability(session, NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT, &caps_async,
                               out_outcome) != FRAME_NVENC_OK) {
        return out_outcome->category;
    }
    if (caps_async != 1) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "The installed hardware encoder does not support asynchronous "
                                "completion events, which this bounded path requires.");
    }
    if (main10) {
        if (frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE, &caps_10bit,
                                   out_outcome) != FRAME_NVENC_OK) return out_outcome->category;
        if (caps_10bit != 1) return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
            "The selected encoder does not report 10-bit encoding support.");
    }
    if (frame_nvenc_capability(session, NV_ENC_CAPS_WIDTH_MAX, &caps_width, out_outcome) !=
        FRAME_NVENC_OK) {
        return out_outcome->category;
    }
    if (frame_nvenc_capability(session, NV_ENC_CAPS_HEIGHT_MAX, &caps_height, out_outcome) !=
        FRAME_NVENC_OK) {
        return out_outcome->category;
    }
    if (caps_width <= 0 || caps_height <= 0 || width > (uint32_t)caps_width ||
        height > (uint32_t)caps_height) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "A %ux%u request exceeds the encoder's reported limit of %dx%d.",
                                width, height, caps_width, caps_height);
    }
    if (frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES, &caps_rc,
                               out_outcome) != FRAME_NVENC_OK) {
        return out_outcome->category;
    }
    if (lossless || rate_control_mode == FRAME_NVENC_RATE_CONTROL_CQP) {
        /* CONSTQP is zero in the SDK and has no bit in the RC capability mask.
         * Lossless and 4:4:4 support were independently queried above. */
    } else if (rate_control_mode == FRAME_NVENC_RATE_CONTROL_VBR) {
        if (((uint32_t)caps_rc & (uint32_t)NV_ENC_PARAMS_RC_VBR) == 0u) {
            return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                    "The installed hardware encoder does not support the VBR rate "
                                    "control mode this request asks for (reported mask %d); no "
                                    "other mode is substituted for it.",
                                    caps_rc);
        }
    } else if (((uint32_t)caps_rc & (uint32_t)NV_ENC_PARAMS_RC_CBR) == 0u) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "The installed hardware encoder does not support the CBR rate "
                                "control mode this request asks for (reported mask %d).",
                                caps_rc);
    }

    status = session->library->functions.nvEncGetInputFormatCount(session->encoder, session->codec_guid, &format_count);
    if (status != NV_ENC_SUCCESS || format_count > 16u) return frame_nvenc_fail(
        out_outcome, FRAME_NVENC_UNSUPPORTED, (int32_t)status, "Input format enumeration failed or exceeded its bound.");
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_FORMATS);
    status = session->library->functions.nvEncGetInputFormats(
        session->encoder, session->codec_guid, formats,
        (uint32_t)(sizeof(formats) / sizeof(formats[0])), &format_count);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        return frame_nvenc_native_failure(out_outcome, "NvEncGetInputFormats", status,
                                          "the selected codec input-format query failed");
    }
    for (index = 0u; index < format_count; ++index) {
        if (formats[index] == NV_ENC_BUFFER_FORMAT_ARGB) input_formats |= 1u;
        if (formats[index] == NV_ENC_BUFFER_FORMAT_ABGR10) input_formats |= 2u;
        if (formats[index] == NV_ENC_BUFFER_FORMAT_NV12) input_formats |= 4u;
        if (formats[index] == NV_ENC_BUFFER_FORMAT_YUV420_10BIT) input_formats |= 8u;
        if (formats[index] == session->input_format) {
            input_argb = 1;
        }
    }
    if (input_argb != 1) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1,
                                "The installed hardware encoder does not accept the requested "
                                "ARGB/ABGR10 input format (reported %u formats).",
                                format_count);
    }

    /* Query hardware facts separately from the recording path's stricter limits. */
    if (frame_nvenc_capability(session, NV_ENC_CAPS_NUM_MAX_BFRAMES, &caps_b_frames, out_outcome) != FRAME_NVENC_OK ||
        frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORT_LOOKAHEAD, &caps_lookahead, out_outcome) != FRAME_NVENC_OK ||
        frame_nvenc_capability(session, NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ, &caps_temporal_aq, out_outcome) != FRAME_NVENC_OK)
        return out_outcome->category;
    if (request->advanced_preset > 7u || request->advanced_tuning > 3u || request->advanced_qp > 51u ||
        request->advanced_multipass > 3u || request->advanced_spatial_aq > 2u || request->advanced_temporal_aq > 2u ||
        (request->advanced_temporal_aq == 2u && caps_temporal_aq != 1) ||
        (rate_control_mode == FRAME_NVENC_RATE_CONTROL_CQP &&
            (bitrate_bps != 0u || max_bitrate_bps != 0u || request->advanced_multipass > 1u ||
             request->advanced_spatial_aq == 2u || request->advanced_temporal_aq == 2u))) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_UNSUPPORTED, -1, "Unsupported native Advanced combination; no substitution.");
    }
    if (request->advanced_preset) {
        const GUID *presets[] = { &NV_ENC_PRESET_P1_GUID, &NV_ENC_PRESET_P2_GUID, &NV_ENC_PRESET_P3_GUID,
            &NV_ENC_PRESET_P4_GUID, &NV_ENC_PRESET_P5_GUID, &NV_ENC_PRESET_P6_GUID, &NV_ENC_PRESET_P7_GUID };
        preset_guid = presets[request->advanced_preset - 1u];
        preset_name = "explicit native preset/tuning";
    }
    if (request->advanced_tuning) tuning = (NV_ENC_TUNING_INFO)request->advanced_tuning;
    (void)memset(&preset_config, 0, sizeof(preset_config));
    preset_config.version = NV_ENC_PRESET_CONFIG_VER;
    preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_PRESET);
    status = session->library->functions.nvEncGetEncodePresetConfigEx(
        session->encoder, session->codec_guid, *preset_guid, tuning, &preset_config);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        return frame_nvenc_fail(out_outcome, frame_nvenc_category(status), (int32_t)status,
                                "NvEncGetEncodePresetConfigEx failed with NVENCSTATUS %d: the "
                                "requested %s preset is not available.",
                                (int32_t)status, preset_name);
    }

    config = preset_config.presetCfg;
    config.version = NV_ENC_CONFIG_VER;
    config.profileGUID = profile;
    config.gopLength = gop_length;
    config.frameIntervalP = 1;
    config.monoChromeEncoding = 0u;
    config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    config.rcParams.version = NV_ENC_RC_PARAMS_VER;
    /* The requested mode is applied exactly: VBR keeps the average bitrate as its target
     * and uses the requested peak, and nothing here silently becomes CBR or the reverse. */
    config.rcParams.rateControlMode = (rate_control_mode == FRAME_NVENC_RATE_CONTROL_VBR)
                                          ? NV_ENC_PARAMS_RC_VBR
                                          : NV_ENC_PARAMS_RC_CBR;
    config.rcParams.averageBitRate = bitrate_bps;
    config.rcParams.maxBitRate = max_bitrate_bps;
    if (lossless) {
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        config.rcParams.constQP.qpInterP = 0u;
        config.rcParams.constQP.qpInterB = 0u;
        config.rcParams.constQP.qpIntra = 0u;
        config.rcParams.averageBitRate = 0u;
        config.rcParams.maxBitRate = 0u;
    }
    if (rate_control_mode == FRAME_NVENC_RATE_CONTROL_CQP) {
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        config.rcParams.constQP.qpInterP = request->advanced_qp;
        config.rcParams.constQP.qpInterB = request->advanced_qp;
        config.rcParams.constQP.qpIntra = request->advanced_qp;
    }
    config.rcParams.vbvBufferSize = 0u;
    config.rcParams.vbvInitialDelay = 0u;
    config.rcParams.enableLookahead = 0u;
    config.rcParams.lookaheadDepth = 0u;
    config.rcParams.enableExtLookahead = 0u;
    config.rcParams.enableAQ = 0u;
    config.rcParams.enableTemporalAQ = 0u;
    config.rcParams.zeroReorderDelay = 1u;
    config.rcParams.enableNonRefP = 0u;
    config.rcParams.strictGOPTarget = 0u;
    config.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
    config.rcParams.qpMapMode = NV_ENC_QP_MAP_DISABLED;
    config.rcParams.lowDelayKeyFrameScale = 1u;
    if (request->advanced_multipass) config.rcParams.multiPass = (NV_ENC_MULTI_PASS)(request->advanced_multipass - 1u);
    config.rcParams.enableAQ = request->advanced_spatial_aq == 2u;
    config.rcParams.aqStrength = 0u; /* Driver chooses strength, no invented user override. */
    config.rcParams.enableTemporalAQ = request->advanced_temporal_aq == 2u;
    if (codec == FRAME_NVENC_CODEC_H264) {
    config.encodeCodecConfig.h264Config.level = (uint32_t)NV_ENC_LEVEL_AUTOSELECT;
    config.encodeCodecConfig.h264Config.idrPeriod = gop_length;
    config.encodeCodecConfig.h264Config.repeatSPSPPS = 1u;
    config.encodeCodecConfig.h264Config.disableSPSPPS = 0u;
    config.encodeCodecConfig.h264Config.chromaFormatIDC = lossless ? 3u : 1u;
    config.encodeCodecConfig.h264Config.qpPrimeYZeroTransformBypassFlag = lossless ? 1u : 0u;
    config.encodeCodecConfig.h264Config.outputAUD = 0u;
    config.encodeCodecConfig.h264Config.outputBufferingPeriodSEI = 0u;
    config.encodeCodecConfig.h264Config.outputPictureTimingSEI = 0u;
    config.encodeCodecConfig.h264Config.outputRecoveryPointSEI = 0u;
    config.encodeCodecConfig.h264Config.enableIntraRefresh = 0u;
    config.encodeCodecConfig.h264Config.enableTemporalSVC = 0u;
    config.encodeCodecConfig.h264Config.hierarchicalPFrames = 0u;
    config.encodeCodecConfig.h264Config.hierarchicalBFrames = 0u;
    config.encodeCodecConfig.h264Config.enableLTR = 0u;
    config.encodeCodecConfig.h264Config.useConstrainedIntraPred = 0u;
    config.encodeCodecConfig.h264Config.numTemporalLayers = 1u;
    config.encodeCodecConfig.h264Config.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
    config.encodeCodecConfig.h264Config.maxNumRefFrames = 1u;
    config.encodeCodecConfig.h264Config.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
    config.encodeCodecConfig.h264Config.numRefL1 = NV_ENC_NUM_REF_FRAMES_1;
    config.encodeCodecConfig.h264Config.outputBitDepth = NV_ENC_BIT_DEPTH_8;
    config.encodeCodecConfig.h264Config.inputBitDepth = NV_ENC_BIT_DEPTH_8;
    config.encodeCodecConfig.h264Config.tfLevel = NV_ENC_TEMPORAL_FILTER_LEVEL_0;
    /* The driver converts the packed BGRA bytes with the BT.709 matrix; signalling the same
     * matrix and a limited (studio) range in the VUI keeps the decoded result consistent.
     * The decoded colour check is what establishes this, not the label. */
    config.encodeCodecConfig.h264Config.h264VUIParameters.videoSignalTypePresentFlag = 1u;
    config.encodeCodecConfig.h264Config.h264VUIParameters.videoFormat =
        (uint32_t)NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    config.encodeCodecConfig.h264Config.h264VUIParameters.videoFullRangeFlag = 0u;
    config.encodeCodecConfig.h264Config.h264VUIParameters.colourDescriptionPresentFlag = 1u;
    config.encodeCodecConfig.h264Config.h264VUIParameters.colourPrimaries =
        (uint32_t)NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    config.encodeCodecConfig.h264Config.h264VUIParameters.transferCharacteristics =
        (uint32_t)NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    config.encodeCodecConfig.h264Config.h264VUIParameters.colourMatrix =
        (uint32_t)NV_ENC_VUI_MATRIX_COEFFS_BT709;
    config.encodeCodecConfig.h264Config.h264VUIParameters.timingInfoPresentFlag = 0u;
    } else if (av1) {
        NV_ENC_CONFIG_AV1 *av = &config.encodeCodecConfig.av1Config;
        /* AV1 level 0 means Level 2.0; its auto value is distinct from AVC/HEVC. */
        av->level = (uint32_t)NV_ENC_LEVEL_AV1_AUTOSELECT;
        av->tier = 0u;
        av->idrPeriod = gop_length;
        av->outputAnnexBFormat = 0u;
        av->enableTimingInfo = 0u;
        av->enableDecoderModelInfo = 0u;
        av->enableFrameIdNumbers = 0u;
        av->repeatSeqHdr = 1u;
        av->disableSeqHdr = 0u;
        av->chromaFormatIDC = 1u;
        av->enableIntraRefresh = 0u;
        av->enableBitstreamPadding = 0u;
        av->enableFilmGrainParams = 0u;
        av->enableTemporalSVC = 0u;
        av->enableLTR = 0u;
        av->numTemporalLayers = 1u;
        av->maxTemporalLayersMinus1 = 0u;
        av->outputBitDepth = main10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        av->inputBitDepth = main10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        av->outputMaxCll = request->content_light_present;
        av->outputMasteringDisplay = request->mastering_present;
        av->colorPrimaries = main10 ? NV_ENC_VUI_COLOR_PRIMARIES_BT2020 : NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        av->transferCharacteristics = main10 ? NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084 : NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        av->matrixCoefficients = main10 ? NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL : NV_ENC_VUI_MATRIX_COEFFS_BT709;
        av->colorRange = 0u;
        av->chromaSamplePosition = 0u;
    } else {
        NV_ENC_CONFIG_HEVC *hevc = &config.encodeCodecConfig.hevcConfig;
        hevc->level = (uint32_t)NV_ENC_LEVEL_AUTOSELECT;
        hevc->idrPeriod = gop_length;
        hevc->repeatSPSPPS = 1u;
        hevc->disableSPSPPS = 0u;
        hevc->chromaFormatIDC = 1u;
        hevc->outputBitDepth = NV_ENC_BIT_DEPTH_10;
        hevc->inputBitDepth = NV_ENC_BIT_DEPTH_10;
        hevc->outputMaxCll = request->content_light_present;
        hevc->outputMasteringDisplay = request->mastering_present;
        hevc->hevcVUIParameters.videoSignalTypePresentFlag = 1u;
        hevc->hevcVUIParameters.videoFormat = (uint32_t)NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        hevc->hevcVUIParameters.videoFullRangeFlag = 0u;
        hevc->hevcVUIParameters.colourDescriptionPresentFlag = 1u;
        hevc->hevcVUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        hevc->hevcVUIParameters.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
        hevc->hevcVUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        hevc->hevcVUIParameters.timingInfoPresentFlag = 0u;
    }

    if (request->colour_present) {
        if (av1) {
            config.encodeCodecConfig.av1Config.colorPrimaries = (NV_ENC_VUI_COLOR_PRIMARIES)request->primaries;
            config.encodeCodecConfig.av1Config.transferCharacteristics = (NV_ENC_VUI_TRANSFER_CHARACTERISTIC)request->transfer;
            config.encodeCodecConfig.av1Config.matrixCoefficients = (NV_ENC_VUI_MATRIX_COEFFS)request->matrix;
            config.encodeCodecConfig.av1Config.colorRange = request->full_range;
        } else {
            NV_ENC_CONFIG_H264_VUI_PARAMETERS *vui = main10
                ? &config.encodeCodecConfig.hevcConfig.hevcVUIParameters
                : &config.encodeCodecConfig.h264Config.h264VUIParameters;
            vui->colourPrimaries = (NV_ENC_VUI_COLOR_PRIMARIES)request->primaries;
            vui->transferCharacteristics = (NV_ENC_VUI_TRANSFER_CHARACTERISTIC)request->transfer;
            vui->colourMatrix = (NV_ENC_VUI_MATRIX_COEFFS)request->matrix;
            vui->videoFullRangeFlag = request->full_range;
        }
    }

    (void)memset(&initialize, 0, sizeof(initialize));
    initialize.version = NV_ENC_INITIALIZE_PARAMS_VER;
    initialize.encodeGUID = session->codec_guid;
    initialize.presetGUID = *preset_guid;
    initialize.encodeWidth = width;
    initialize.encodeHeight = height;
    initialize.darWidth = width;
    initialize.darHeight = height;
    initialize.frameRateNum = rate_num;
    initialize.frameRateDen = rate_den;
    /* ADR 0006 review addendum: asynchronous completion with outputs in system memory. */
    initialize.enableEncodeAsync = 1u;
    initialize.enablePTD = 1u;
    initialize.enableOutputInVidmem = 0u;
    initialize.tuningInfo = tuning;
    initialize.bufferFormat = session->input_format;
    initialize.encodeConfig = &config;
    initialize.maxEncodeWidth = 0u;
    initialize.maxEncodeHeight = 0u;
    initialize.numStateBuffers = 0u;

    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_INITIALIZE);
    status = session->library->functions.nvEncInitializeEncoder(session->encoder, &initialize);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        return frame_nvenc_native_failure(out_outcome, "NvEncInitializeEncoder", status,
                                          "the requested configuration was rejected");
    }
    session->initialized = 1;

    (void)memset(sequence, 0, sizeof(sequence));
    (void)memset(&payload, 0, sizeof(payload));
    payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
    payload.inBufferSize = (uint32_t)sizeof(sequence);
    payload.spsppsBuffer = sequence;
    payload.outSPSPPSPayloadSize = &sequence_bytes;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_SEQUENCE);
    status = session->library->functions.nvEncGetSequenceParams(session->encoder, &payload);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        return frame_nvenc_native_failure(out_outcome, "NvEncGetSequenceParams", status,
                                          "the codec initialization bytes could not be read");
    }
    if (sequence_bytes == 0u || sequence_bytes > (uint32_t)sizeof(sequence)) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, (int32_t)status,
                                "NvEncGetSequenceParams reported %u initialization bytes, which is "
                                "outside the bounded 1..%u range.",
                                sequence_bytes, (uint32_t)sizeof(sequence));
    }

    (void)memset(&session->settings, 0, sizeof(session->settings));
    session->settings.width = width;
    session->settings.height = height;
    session->settings.rate_num = rate_num;
    session->settings.rate_den = rate_den;
    session->settings.bitrate_bps = config.rcParams.averageBitRate;
    session->settings.max_bitrate_bps = config.rcParams.maxBitRate;
    session->settings.gop_length = config.gopLength;
    session->settings.idr_period = av1 ? config.encodeCodecConfig.av1Config.idrPeriod : (codec == FRAME_NVENC_CODEC_HEVC_MAIN10
        ? config.encodeCodecConfig.hevcConfig.idrPeriod
        : config.encodeCodecConfig.h264Config.idrPeriod);
    session->settings.frame_interval_p = 1u;
    session->settings.enable_ptd = initialize.enablePTD;
    session->settings.enable_async = initialize.enableEncodeAsync;
    session->settings.repeat_spspps = av1 ? config.encodeCodecConfig.av1Config.repeatSeqHdr : (codec == FRAME_NVENC_CODEC_HEVC_MAIN10
        ? config.encodeCodecConfig.hevcConfig.repeatSPSPPS
        : config.encodeCodecConfig.h264Config.repeatSPSPPS);
    session->settings.level = av1 ? config.encodeCodecConfig.av1Config.level : (codec == FRAME_NVENC_CODEC_HEVC_MAIN10
        ? config.encodeCodecConfig.hevcConfig.level
        : config.encodeCodecConfig.h264Config.level);
    session->settings.profile_high = codec == FRAME_NVENC_CODEC_H264;
    session->settings.vui_full_range = av1 ? config.encodeCodecConfig.av1Config.colorRange : (codec == FRAME_NVENC_CODEC_HEVC_MAIN10
        ? config.encodeCodecConfig.hevcConfig.hevcVUIParameters.videoFullRangeFlag
        : config.encodeCodecConfig.h264Config.h264VUIParameters.videoFullRangeFlag);
    session->settings.rate_control_mode = lossless ? FRAME_NVENC_RATE_CONTROL_LOSSLESS :
        (config.rcParams.rateControlMode == NV_ENC_PARAMS_RC_CONSTQP ? FRAME_NVENC_RATE_CONTROL_CQP :
        (config.rcParams.rateControlMode == NV_ENC_PARAMS_RC_VBR ? FRAME_NVENC_RATE_CONTROL_VBR : FRAME_NVENC_RATE_CONTROL_CBR));
    session->settings.preset = preset;
    session->settings.caps_async_encode = (uint32_t)caps_async;
    session->settings.caps_width_max = (uint32_t)caps_width;
    session->settings.caps_height_max = (uint32_t)caps_height;
    session->settings.caps_rate_control_modes = (uint32_t)caps_rc;
    session->settings.caps_h264_guid = codec == FRAME_NVENC_CODEC_H264;
    session->settings.caps_input_argb = (input_formats & 1u) != 0u;
    /* The driver accepted the H.264 GUID in the capability queries above and initialized the
     * H.264 High session, which is what validates the profile. */
    session->settings.caps_profile_high = codec == FRAME_NVENC_CODEC_H264;
    session->settings.caps_preset_available = 1u;
    session->settings.codec_config_bytes = sequence_bytes;
    (void)memcpy(session->settings.codec_config, sequence, (size_t)sequence_bytes);
    session->settings.codec = codec;
    session->settings.caps_10bit = (uint32_t)caps_10bit;
    session->settings.caps_input_abgr10 = (input_formats & 2u) != 0u;
    session->settings.input_format = request->input_format;
    session->settings.caps_input_formats = input_formats;
    session->settings.caps_profile = 1u;
    session->settings.advanced[0] = request->advanced_preset ? request->advanced_preset :
        (preset == FRAME_NVENC_PRESET_HIGH_QUALITY ? 5u : (preset == FRAME_NVENC_PRESET_MAXIMUM_QUALITY ? 7u : 1u));
    session->settings.advanced[1] = (uint32_t)tuning;
    session->settings.advanced[2] = config.rcParams.constQP.qpIntra;
    session->settings.advanced[3] = (uint32_t)config.rcParams.multiPass;
    session->settings.advanced[4] = config.rcParams.enableAQ;
    session->settings.advanced[5] = config.rcParams.enableTemporalAQ;
    session->settings.advanced[6] = (uint32_t)caps_b_frames;
    session->settings.advanced[7] = (uint32_t)caps_lookahead;
    session->settings.advanced[8] = (uint32_t)caps_temporal_aq;
    session->mastering_present = request->mastering_present;
    session->content_light_present = request->content_light_present;
    session->mastering.r.x = (uint16_t)request->chromaticity[0];
    session->mastering.r.y = (uint16_t)request->chromaticity[1];
    session->mastering.g.x = (uint16_t)request->chromaticity[2];
    session->mastering.g.y = (uint16_t)request->chromaticity[3];
    session->mastering.b.x = (uint16_t)request->chromaticity[4];
    session->mastering.b.y = (uint16_t)request->chromaticity[5];
    session->mastering.whitePoint.x = (uint16_t)request->chromaticity[6];
    session->mastering.whitePoint.y = (uint16_t)request->chromaticity[7];
    session->mastering.maxLuma = request->luminance_max;
    session->mastering.minLuma = request->luminance_min;
    session->content_light.maxContentLightLevel = (uint16_t)request->max_cll;
    session->content_light.maxPicAverageLightLevel = (uint16_t)request->max_fall;
    return FRAME_NVENC_OK;
}

/* ------------------------------------------------------------ open and probe */

static int32_t frame_nvenc_open_session(FrameNvencLibrary *library, void *d3d11_device,
                                        const FrameNvencSessionRequest *request,
                                        FrameNvencSession **out_session,
                                        FrameNvencOutcome *out_outcome) {
    FrameNvencSession *session;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open;
    NVENCSTATUS status;

    if (library == NULL || d3d11_device == NULL || request == NULL || out_session == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Opening an encoder session needs a loaded library, a Direct3D "
                                "device and a request.");
    }
    *out_session = NULL;
    session = (FrameNvencSession *)calloc(1u, sizeof(FrameNvencSession));
    if (session == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "The bounded encoder session record could not be allocated.");
    }
    session->library = library;
    session->witness_id = frame_witness_create("encode/preparation");
    if (session->witness_id == 0u) {
        free(session);
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1, "The bounded 64-session witness registry is exhausted.");
    }

    (void)memset(&open, 0, sizeof(open));
    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    open.device = d3d11_device;
    open.apiVersion = (uint32_t)NVENCAPI_VERSION;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_OPEN);
    status = library->functions.nvEncOpenEncodeSessionEx(&open, &session->encoder);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS || session->encoder == NULL) {
        frame_witness_release(session->witness_id);
        (void)free(session);
        return frame_nvenc_native_failure(
            out_outcome, "NvEncOpenEncodeSessionEx", status,
            "the capture device could not host an NVENC session; this path has no software or "
            "other-adapter fallback");
    }

    if (frame_nvenc_configure(session, request->width, request->height, request->rate_num,
                              request->rate_den, request->bitrate_bps, request->max_bitrate_bps,
                              request->gop_length, request->rate_control_mode, request->preset,
                              request->codec, request,
                              out_outcome) != FRAME_NVENC_OK) {
        /* Partial initialization: no output buffer, event or registration exists yet, so only
         * the open session has to go. */
        (void)frame_nvenc_close_owned(session, NULL);
        return out_outcome->category;
    }
    *out_session = session;
    return FRAME_NVENC_OK;
}

int32_t frame_nvenc_probe(FrameNvencLibrary *library, void *d3d11_device,
                          const FrameNvencSessionRequest *request,
                          FrameNvencSettings *out_settings, FrameNvencOutcome *out_outcome) {
    FrameNvencSession *session = NULL;
    int32_t result;
    FrameNvencOutcome close_outcome;

    frame_nvenc_clear(out_outcome);
    if (out_settings == NULL || out_outcome == NULL || request == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "frame_nvenc_probe needs a request and settings and outcome "
                                "out-parameters.");
    }
    (void)memset(out_settings, 0, sizeof(*out_settings));

    result = frame_nvenc_open_session(library, d3d11_device, request, &session, out_outcome);
    if (result != FRAME_NVENC_OK) {
        return result;
    }
    *out_settings = session->settings;
    /* A qualification probe allocates no native output buffers; it reports the plan. */
    out_settings->output_buffers = FRAME_NVENC_OUTPUT_BUFFERS;
    out_settings->output_buffer_capacity = frame_nvenc_output_capacity();
    out_settings->max_access_unit_bytes = FRAME_NVENC_MAX_ACCESS_UNIT_BYTES;

    frame_nvenc_clear(&close_outcome);
    if (frame_nvenc_close_owned(session, &close_outcome) != FRAME_NVENC_OK) {
        return frame_nvenc_fail(out_outcome, close_outcome.category, close_outcome.status,
                                "The qualification session initialized but cleanup failed: %s",
                                close_outcome.detail);
    }
    return frame_nvenc_ok(out_outcome,
                          "Qualified %ux%u at %u/%u: %s, %s %u bit/s (peak %u), GOP %u, "
                          "preset %u, asynchronous events, %u initialization bytes.",
                          request->width, request->height, request->rate_num, request->rate_den,
                          request->codec == FRAME_NVENC_CODEC_HEVC_MAIN10 ? "HEVC Main10" :
                              (request->codec == FRAME_NVENC_CODEC_AV1 ? "AV1 Main8" :
                              (request->codec == FRAME_NVENC_CODEC_AV1_MAIN10 ? "AV1 Main10" : "H.264 High")),
                          request->rate_control_mode == FRAME_NVENC_RATE_CONTROL_VBR ? "VBR" :
                              (request->rate_control_mode == FRAME_NVENC_RATE_CONTROL_CQP ? "CQP" :
                              (request->rate_control_mode == FRAME_NVENC_RATE_CONTROL_LOSSLESS ? "Lossless QP 0" : "CBR")),
                          request->bitrate_bps, request->max_bitrate_bps, request->gop_length,
                          request->preset, out_settings->codec_config_bytes);
}

int32_t frame_nvenc_session_open(FrameNvencLibrary *library, void *d3d11_device,
                                 const FrameNvencSessionRequest *request,
                                 FrameNvencSession **out_session,
                                 FrameNvencSettings *out_settings,
                                 FrameNvencOutcome *out_outcome) {
    FrameNvencSession *session = NULL;
    int32_t result;
    uint32_t index;
    uint32_t capacity;
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (out_settings == NULL || out_outcome == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "frame_nvenc_session_open needs settings and outcome "
                                "out-parameters.");
    }
    (void)memset(out_settings, 0, sizeof(*out_settings));
    result = frame_nvenc_open_session(library, d3d11_device, request, &session, out_outcome);
    if (result != FRAME_NVENC_OK) {
        return result;
    }

    capacity = frame_nvenc_output_capacity();
    for (index = 0u; index < FRAME_NVENC_OUTPUT_BUFFERS; ++index) {
        NV_ENC_CREATE_BITSTREAM_BUFFER create;
        (void)memset(&create, 0, sizeof(create));
        create.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        create.size = capacity;
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_CREATE_BITSTREAM);
        status = session->library->functions.nvEncCreateBitstreamBuffer(session->encoder, &create);
        frame_nvenc_phase_end(session);
        if (status != NV_ENC_SUCCESS || create.bitstreamBuffer == NULL) {
            (void)frame_nvenc_native_failure(
                out_outcome, "NvEncCreateBitstreamBuffer", status,
                "a bounded output bitstream buffer could not be created");
            (void)frame_nvenc_close_owned(session, NULL);
            return out_outcome->category;
        }
        session->outputs[index] = create.bitstreamBuffer;
    }

    /* Manual-reset events: the driver signals them and Frame resets them after use. */
    for (index = 0u; index < FRAME_NVENC_OUTPUT_BUFFERS; ++index) {
        NV_ENC_EVENT_PARAMS registration;
        session->events[index] = (void *)CreateEventW(NULL, TRUE, FALSE, NULL);
        if (session->events[index] == NULL) {
            (void)frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                   "A completion event for output buffer %u could not be created "
                                   "(Win32 error %lu).",
                                   index, (unsigned long)GetLastError());
            (void)frame_nvenc_close_owned(session, NULL);
            return out_outcome->category;
        }
        (void)memset(&registration, 0, sizeof(registration));
        registration.version = NV_ENC_EVENT_PARAMS_VER;
        registration.completionEvent = session->events[index];
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_REGISTER_EVENT);
        status = session->library->functions.nvEncRegisterAsyncEvent(session->encoder,
                                                                    &registration);
        frame_nvenc_phase_end(session);
        if (status != NV_ENC_SUCCESS) {
            (void)frame_nvenc_native_failure(out_outcome, "NvEncRegisterAsyncEvent", status,
                                             "a completion event could not be registered");
            (void)frame_nvenc_close_owned(session, NULL);
            return out_outcome->category;
        }
        session->event_registered[index] = 1;
    }

    /* A separate EOS event, so drain completion is never inferred from a picture event. */
    session->eos_event = (void *)CreateEventW(NULL, TRUE, FALSE, NULL);
    if (session->eos_event == NULL) {
        (void)frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                               "The separate end-of-stream event could not be created (Win32 "
                               "error %lu).",
                               (unsigned long)GetLastError());
        (void)frame_nvenc_close_owned(session, NULL);
        return out_outcome->category;
    }
    {
        NV_ENC_EVENT_PARAMS registration;
        (void)memset(&registration, 0, sizeof(registration));
        registration.version = NV_ENC_EVENT_PARAMS_VER;
        registration.completionEvent = session->eos_event;
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_REGISTER_EVENT);
        status = session->library->functions.nvEncRegisterAsyncEvent(session->encoder,
                                                                    &registration);
        frame_nvenc_phase_end(session);
        if (status != NV_ENC_SUCCESS) {
            (void)frame_nvenc_native_failure(out_outcome, "NvEncRegisterAsyncEvent", status,
                                             "the end-of-stream event could not be registered");
            (void)frame_nvenc_close_owned(session, NULL);
            return out_outcome->category;
        }
        session->eos_registered = 1;
    }

    *out_session = session;
    /* The session's own record keeps the same bounds, because collection checks a locked
     * access unit against the native buffer capacity. */
    session->settings.output_buffers = FRAME_NVENC_OUTPUT_BUFFERS;
    session->settings.output_buffer_capacity = capacity;
    session->settings.max_access_unit_bytes = FRAME_NVENC_MAX_ACCESS_UNIT_BYTES;
    *out_settings = session->settings;
    return FRAME_NVENC_OK;
}

/* ----------------------------------------------------- registration and input */

int32_t frame_nvenc_register_input(FrameNvencSession *session, void *d3d11_texture,
                                   uint32_t width, uint32_t height,
                                   uint32_t *out_registration, FrameNvencOutcome *out_outcome) {
    uint32_t index;
    uint32_t free_index = FRAME_NVENC_INPUT_REGISTRATIONS;
    NV_ENC_REGISTER_RESOURCE registration;
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (session == NULL || d3d11_texture == NULL || out_registration == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Registering an input needs a session, a texture and an index "
                                "out-parameter.");
    }
    *out_registration = 0u;
    if (session->initialized == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "An input texture cannot be registered before the encoder session "
                                "is initialized.");
    }
    if (width != session->settings.width || height != session->settings.height) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "A %ux%u texture cannot be registered with a %ux%u encode session; "
                                "this encoder rejects a dimension change instead of reconfiguring.",
                                width, height, session->settings.width, session->settings.height);
    }
    for (index = 0u; index < FRAME_NVENC_INPUT_REGISTRATIONS; ++index) {
        if (session->inputs[index].used != 0 && session->inputs[index].texture == d3d11_texture) {
            *out_registration = index;
            return frame_nvenc_ok(out_outcome, "Input %u is already registered.", index);
        }
        if (session->inputs[index].used == 0 && free_index == FRAME_NVENC_INPUT_REGISTRATIONS) {
            free_index = index;
        }
    }
    if (free_index == FRAME_NVENC_INPUT_REGISTRATIONS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "All %u bounded input registrations are in use; this encoder never "
                                "registers a fifth capture texture.",
                                (uint32_t)FRAME_NVENC_INPUT_REGISTRATIONS);
    }

    (void)memset(&registration, 0, sizeof(registration));
    registration.version = NV_ENC_REGISTER_RESOURCE_VER;
    registration.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    registration.width = width;
    registration.height = height;
    registration.pitch = 0u;
    registration.subResourceIndex = 0u;
    registration.resourceToRegister = d3d11_texture;
    registration.bufferFormat = session->input_format;
    registration.bufferUsage = NV_ENC_INPUT_IMAGE;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_REGISTER_RESOURCE);
    status = session->library->functions.nvEncRegisterResource(session->encoder, &registration);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS || registration.registeredResource == NULL) {
        return frame_nvenc_native_failure(
            out_outcome, "NvEncRegisterResource", status,
            "the capture texture was rejected as a Direct3D input image");
    }
    session->inputs[free_index].registered = registration.registeredResource;
    session->inputs[free_index].texture = d3d11_texture;
    session->inputs[free_index].width = width;
    session->inputs[free_index].height = height;
    session->inputs[free_index].used = 1;
    session->inputs[free_index].mapped = NULL;
    *out_registration = free_index;
    return frame_nvenc_ok(out_outcome, "Registered capture texture as input %u.", free_index);
}

int32_t frame_nvenc_unregister_input(FrameNvencSession *session, uint32_t registration,
                                     FrameNvencOutcome *out_outcome) {
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (session == NULL || registration >= FRAME_NVENC_INPUT_REGISTRATIONS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Unregistering needs a session and one of its bounded input "
                                "registration indices.");
    }
    if (session->inputs[registration].used == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Input registration %u is not in use.", registration);
    }
    if (session->inputs[registration].mapped != NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Input registration %u is still mapped for an in-flight picture "
                                "and must not be unregistered.",
                                registration);
    }
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNREGISTER_RESOURCE);
    status = session->library->functions.nvEncUnregisterResource(
        session->encoder, (NV_ENC_REGISTERED_PTR)session->inputs[registration].registered);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        /* The registration is still live, so it stays recorded and the failure is reported
         * instead of being counted as a successful release. */
        return frame_nvenc_native_failure(out_outcome, "NvEncUnregisterResource", status,
                                          "the capture texture registration was not released");
    }
    session->inputs[registration].used = 0;
    session->inputs[registration].registered = NULL;
    session->inputs[registration].texture = NULL;
    return frame_nvenc_ok(out_outcome, "Unregistered input %u.", registration);
}

/* Frees one output buffer: unmaps the input that this submission mapped and resets its event.
 * Runs on completion, on every failure and during teardown, so it is idempotent per slot and
 * releases exactly the mapping that belongs to it. */
static NVENCSTATUS frame_nvenc_release_output(FrameNvencSession *session, uint32_t slot,
                                       uint32_t *out_cleanup_failures) {
    uint32_t registration;
    if (slot >= FRAME_NVENC_OUTPUT_BUFFERS || session->output_in_use[slot] == 0) {
        return NV_ENC_SUCCESS;
    }
    registration = session->output_registration[slot];
    if (registration < FRAME_NVENC_INPUT_REGISTRATIONS &&
        session->inputs[registration].mapped != NULL) {
        NVENCSTATUS status;
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNMAP);
        status = session->library->functions.nvEncUnmapInputResource(
            session->encoder, session->inputs[registration].mapped);
        frame_nvenc_phase_end(session);
        if (status != NV_ENC_SUCCESS) {
            /* The mapping could not be released, so the driver may still read this
             * texture: keep the mapping recorded and the slot owned instead of clearing
             * state that no longer matches reality. */
            if (out_cleanup_failures != NULL) {
                *out_cleanup_failures += 1u;
            }
            return status;
        }
        session->inputs[registration].mapped = NULL;
        session->pending_inputs =
            session->pending_inputs > 0u ? session->pending_inputs - 1u : 0u;
    }
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_RESET_EVENT);
    (void)ResetEvent((HANDLE)session->events[slot]);
    frame_nvenc_phase_end(session);
    session->output_in_use[slot] = 0;
    session->output_complete[slot] = 0;
    return NV_ENC_SUCCESS;
}

/*
 * Establishes that the driver is finished with one owned output before its resources are
 * recycled. It waits for that output's completion event within a bounded budget and, when
 * it signals, locks and unlocks the bitstream so the driver's write has definitely ended.
 * Returns 1 when the output is safe to release, 0 when it is not (the caller must then
 * keep the resources represented).
 */
static int32_t frame_nvenc_settle_output(FrameNvencSession *session, uint32_t slot,
                                         uint64_t deadline_tick, NVENCSTATUS *failed_status, const char **failed_api) {
    DWORD wait_result = WAIT_TIMEOUT;
    NV_ENC_LOCK_BITSTREAM lock;
    NVENCSTATUS status;
    if (slot >= FRAME_NVENC_OUTPUT_BUFFERS || session->output_in_use[slot] == 0) {
        return 1;
    }
    if (session->output_complete[slot]) return 1;
    if (session->output_locked[slot]) goto unlock;
    /* One monotonic deadline shared by every owned output, and bounded polling inside it:
     * each wait is at most FRAME_NVENC_SETTLE_POLL_MS *or* the remaining time, whichever is
     * smaller, so an overshoot cannot extend the deadline per slot. Expiring the deadline
     * means native use is unknown, not that the output may be destroyed. */
    for (;;) {
        uint64_t now = GetTickCount64();
        uint32_t remaining;
        if (now >= deadline_tick) {
            return 0;
        }
        remaining = (uint32_t)(deadline_tick - now);
        if (remaining > (uint32_t)FRAME_NVENC_SETTLE_POLL_MS) {
            remaining = (uint32_t)FRAME_NVENC_SETTLE_POLL_MS;
        }
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_WAIT_EVENT);
        wait_result = WaitForSingleObject((HANDLE)session->events[slot], (DWORD)remaining);
        frame_nvenc_phase_end(session);
        if (wait_result == WAIT_OBJECT_0) {
            break;
        }
        if (wait_result != WAIT_TIMEOUT) {
            return 0;
        }
    }
    (void)memset(&lock, 0, sizeof(lock));
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = (void *)session->outputs[slot];
    lock.doNotWait = 0u;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_LOCK);
    status = session->library->functions.nvEncLockBitstream(session->encoder, &lock);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS) {
        if (*failed_status == NV_ENC_SUCCESS) { *failed_status = status; *failed_api = "NvEncLockBitstream"; }
        return 0;
    }
    session->output_locked[slot] = 1;
unlock:
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNLOCK);
    status = session->library->functions.nvEncUnlockBitstream(
        session->encoder, (NV_ENC_OUTPUT_PTR)session->outputs[slot]);
    frame_nvenc_phase_end(session);
    if (status == NV_ENC_SUCCESS) { session->output_locked[slot] = 0; session->output_complete[slot] = 1; }
    if (status != NV_ENC_SUCCESS && *failed_status == NV_ENC_SUCCESS) { *failed_status = status; *failed_api = "NvEncUnlockBitstream"; }
    return status == NV_ENC_SUCCESS ? 1 : 0;
}

int32_t frame_nvenc_submit(FrameNvencSession *session, uint32_t registration, uint64_t pts_ns,
                           uint64_t duration_ns, uint32_t force_idr, uint32_t *out_slot,
                           uint32_t *out_buffered, FrameNvencOutcome *out_outcome) {
    uint32_t slot;
    uint32_t free_slot = FRAME_NVENC_OUTPUT_BUFFERS;
    NV_ENC_MAP_INPUT_RESOURCE map;
    NV_ENC_PIC_PARAMS picture;
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (session == NULL || out_slot == NULL || out_buffered == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Submitting a picture needs a session and its out-parameters.");
    }
    *out_slot = 0u;
    *out_buffered = 0u;
    if (session->initialized == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "A picture cannot be submitted before the encoder session is "
                                "initialized.");
    }
    if (registration >= FRAME_NVENC_INPUT_REGISTRATIONS || session->inputs[registration].used == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Input registration %u is not registered with this session.",
                                registration);
    }
    if (session->inputs[registration].mapped != NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Input registration %u is still mapped by an incomplete picture.",
                                registration);
    }
    for (slot = 0u; slot < FRAME_NVENC_OUTPUT_BUFFERS; ++slot) {
        if (session->output_in_use[slot] == 0) {
            free_slot = slot;
            break;
        }
    }
    if (free_slot == FRAME_NVENC_OUTPUT_BUFFERS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "All %u bounded output buffers are in flight; this encoder never "
                                "allocates another one.",
                                (uint32_t)FRAME_NVENC_OUTPUT_BUFFERS);
    }

    (void)memset(&map, 0, sizeof(map));
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource =
        (NV_ENC_REGISTERED_PTR)session->inputs[registration].registered;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_MAP);
    status = session->library->functions.nvEncMapInputResource(session->encoder, &map);
    frame_nvenc_phase_end(session);
    if (status != NV_ENC_SUCCESS || map.mappedResource == NULL) {
        return frame_nvenc_native_failure(out_outcome, "NvEncMapInputResource", status,
                                          "the registered capture texture could not be mapped for "
                                          "encoding");
    }
    session->inputs[registration].mapped = map.mappedResource;
    session->pending_inputs += 1u;

    (void)memset(&picture, 0, sizeof(picture));
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = session->settings.width;
    picture.inputHeight = session->settings.height;
    picture.inputPitch = session->settings.width;
    picture.encodePicFlags = force_idr != 0u ? (uint32_t)NV_ENC_PIC_FLAG_FORCEIDR : 0u;
    picture.frameIdx = (uint32_t)session->submitted_frames;
    picture.inputTimeStamp = pts_ns;
    picture.inputDuration = duration_ns;
    picture.inputBuffer = map.mappedResource;
    picture.outputBitstream = (NV_ENC_OUTPUT_PTR)session->outputs[free_slot];
    picture.completionEvent = session->events[free_slot];
    picture.bufferFmt = map.mappedBufferFmt;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    /* Repeat source metadata with every picture so every independent segment
     * and replay start carries the same signal. No nominal mastering values. */
    if (session->codec == FRAME_NVENC_CODEC_HEVC_MAIN10) {
        picture.codecPicParams.hevcPicParams.pMasteringDisplay = session->mastering_present ? &session->mastering : NULL;
        picture.codecPicParams.hevcPicParams.pMaxCll = session->content_light_present ? &session->content_light : NULL;
    } else if (session->codec == FRAME_NVENC_CODEC_AV1 || session->codec == FRAME_NVENC_CODEC_AV1_MAIN10) {
        picture.codecPicParams.av1PicParams.pMasteringDisplay = session->mastering_present ? &session->mastering : NULL;
        picture.codecPicParams.av1PicParams.pMaxCll = session->content_light_present ? &session->content_light : NULL;
    }
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_ENCODE);
    status = session->library->functions.nvEncEncodePicture(session->encoder, &picture);
    frame_nvenc_phase_end(session);

    if (status == NV_ENC_ERR_NEED_MORE_INPUT) {
        /* Buffered acceptance: the driver took the picture, kept the input mapped and will write
         * this output buffer when the stream advances. The slot and its event stay owned until
         * that access unit has been collected, so this is an outstanding submission and never a
         * released input. */
        session->output_in_use[free_slot] = 1;
        session->output_registration[free_slot] = registration;
        session->submitted_frames += 1u;
        session->buffered_pictures += 1u;
        *out_slot = free_slot;
        *out_buffered = 1u;
        return frame_nvenc_ok(out_outcome,
                              "Picture %llu was buffered by the driver (NEED_MORE_INPUT) on output "
                              "buffer %u; its access unit arrives with later input or EOS.",
                              (unsigned long long)(session->submitted_frames - 1u), free_slot);
    }

    if (status != NV_ENC_SUCCESS) {
        NVENCSTATUS unmap_status;
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNMAP);
        unmap_status = session->library->functions.nvEncUnmapInputResource(
            session->encoder, session->inputs[registration].mapped);
        frame_nvenc_phase_end(session);
        if (unmap_status != NV_ENC_SUCCESS) {
            return frame_nvenc_fail(
                out_outcome, FRAME_NVENC_FAILED, (int32_t)status,
                "NvEncEncodePicture failed with NVENCSTATUS %d and the input unmap then failed with "
                "NVENCSTATUS %d.",
                (int32_t)status, (int32_t)unmap_status);
        }
        session->inputs[registration].mapped = NULL;
        session->pending_inputs = session->pending_inputs > 0u ? session->pending_inputs - 1u : 0u;
        if (status == NV_ENC_ERR_ENCODER_BUSY) {
            session->busy_results += 1u;
            return frame_nvenc_fail(out_outcome, FRAME_NVENC_BUSY, (int32_t)status,
                                    "NvEncEncodePicture reported ENCODER_BUSY: nothing was accepted "
                                    "and the input must be retried unchanged.");
        }
        {
            const char *last = session->library->functions.nvEncGetLastErrorString(session->encoder);
            return frame_nvenc_fail(out_outcome, frame_nvenc_category(status), (int32_t)status,
                                    "NvEncEncodePicture failed with NVENCSTATUS %d (%s).",
                                    (int32_t)status, last != NULL ? last : "no detail");
        }
    }

    session->output_in_use[free_slot] = 1;
    session->output_registration[free_slot] = registration;
    session->submitted_frames += 1u;
    *out_slot = free_slot;
    return frame_nvenc_ok(out_outcome, "Picture %llu submitted on output buffer %u.",
                          (unsigned long long)(session->submitted_frames - 1u), free_slot);
}

int32_t frame_nvenc_collect(FrameNvencSession *session, uint32_t slot, uint32_t wait_ms,
                            uint8_t *destination, uint32_t destination_capacity,
                            FrameNvencPacketInfo *out_info, FrameNvencOutcome *out_outcome) {
    DWORD wait_result;
    NV_ENC_LOCK_BITSTREAM lock;
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (session == NULL || out_info == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Collecting a picture needs a session and an information "
                                "out-parameter.");
    }
    (void)memset(out_info, 0, sizeof(*out_info));
    out_info->state = FRAME_NVENC_STATE_PENDING;
    if (slot >= FRAME_NVENC_OUTPUT_BUFFERS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Output buffer %u is outside the bounded %u-buffer set.", slot,
                                (uint32_t)FRAME_NVENC_OUTPUT_BUFFERS);
    }
    if (session->output_in_use[slot] == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Output buffer %u has no submitted picture to collect.", slot);
    }
    if (destination == NULL || destination_capacity == 0u) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Collecting a picture needs a bounded destination buffer.");
    }

    frame_nvenc_phase_begin(session, FRAME_NVENC_API_WAIT_EVENT);
    wait_result = WaitForSingleObject((HANDLE)session->events[slot], (DWORD)wait_ms);
    frame_nvenc_phase_end(session);
    if (wait_result == WAIT_TIMEOUT) {
        return frame_nvenc_ok(out_outcome, "Output buffer %u has not completed within %u ms.", slot,
                              wait_ms);
    }
    if (wait_result != WAIT_OBJECT_0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "Waiting for output buffer %u failed with Win32 result %lu.", slot,
                                (unsigned long)wait_result);
    }

    (void)memset(&lock, 0, sizeof(lock));
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = (void *)session->outputs[slot];
    /* ADR 0006 review addendum: this output's completion event has already signalled, so the
     * lock is allowed to block on an encoder that is still finishing its write. */
    lock.doNotWait = 0u;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_LOCK);
    status = session->library->functions.nvEncLockBitstream(session->encoder, &lock);
    frame_nvenc_phase_end(session);
    if (status == NV_ENC_ERR_LOCK_BUSY || status == NV_ENC_ERR_ENCODER_BUSY) {
        /* Keep the mapping and the slot: the next bounded poll retries the lock. */
        session->busy_results += 1u;
        return frame_nvenc_ok(out_outcome,
                              "Output buffer %u signalled but is not lockable yet (NVENCSTATUS %d).",
                              slot, (int32_t)status);
    }
    if (status != NV_ENC_SUCCESS) {
        const char *last = session->library->functions.nvEncGetLastErrorString(session->encoder);
        return frame_nvenc_fail(out_outcome, frame_nvenc_category(status), (int32_t)status,
                                "NvEncLockBitstream failed with NVENCSTATUS %d (%s).",
                                (int32_t)status, last != NULL ? last : "no detail");
    }

    session->output_locked[slot] = 1;
    out_info->bytes = lock.bitstreamSizeInBytes;
    out_info->average_qp = lock.frameAvgQP;
    out_info->pts_ns = lock.outputTimeStamp;
    out_info->duration_ns = lock.outputDuration;
    out_info->frame_index = (uint64_t)lock.frameIdx;
    out_info->picture_type = (uint32_t)lock.pictureType;
    out_info->keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR ? 1u : 0u;

    if (lock.bitstreamBufferPtr == NULL || lock.bitstreamSizeInBytes == 0u) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, (int32_t)status,
                                "NvEncLockBitstream reported %u bytes at a null pointer for output "
                                "buffer %u; an empty access unit is never delivered.",
                                lock.bitstreamSizeInBytes, slot);
    }
    /* A unit that does not fit, or that exactly fills the native buffer, may have been
     * truncated by the driver: refuse it instead of delivering a partial bitstream. */
    if (lock.bitstreamSizeInBytes > destination_capacity ||
        lock.bitstreamSizeInBytes >= session->settings.output_buffer_capacity) {
        uint32_t oversized = lock.bitstreamSizeInBytes;
        uint32_t native_capacity = session->settings.output_buffer_capacity;
        return frame_nvenc_fail(
            out_outcome, FRAME_NVENC_FAILED, (int32_t)status,
            "The encoder produced %u bytes for output buffer %u, which exceeds the %u-byte access "
            "unit bound or exactly fills the %u-byte native buffer and may have been truncated; "
            "this bitstream is refused rather than truncated.",
            oversized, slot, destination_capacity, native_capacity);
    }

    (void)memcpy(destination, lock.bitstreamBufferPtr, (size_t)lock.bitstreamSizeInBytes);
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNLOCK);
    status = session->library->functions.nvEncUnlockBitstream(
        session->encoder, (NV_ENC_OUTPUT_PTR)session->outputs[slot]);
    frame_nvenc_phase_end(session);
    if (status == NV_ENC_SUCCESS) { session->output_locked[slot] = 0; session->output_complete[slot] = 1; }
    {
        uint32_t cleanup_failures = 0u;
        if (status == NV_ENC_SUCCESS) status = frame_nvenc_release_output(session, slot, &cleanup_failures);
        if (status != NV_ENC_SUCCESS || cleanup_failures != 0u) {
            return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, (int32_t)status,
                                    "The access unit was copied, but releasing output buffer %u "
                                    "failed (NVENCSTATUS %d, %u cleanup failures).",
                                    slot, (int32_t)status, cleanup_failures);
        }
    }
    out_info->state = FRAME_NVENC_STATE_READY;
    return frame_nvenc_ok(out_outcome, "Collected %u bytes from output buffer %u.", out_info->bytes,
                          slot);
}

int32_t frame_nvenc_begin_eos(FrameNvencSession *session, FrameNvencOutcome *out_outcome) {
    NV_ENC_PIC_PARAMS picture;
    NVENCSTATUS status;

    frame_nvenc_clear(out_outcome);
    if (session == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1, "End of stream needs a session.");
    }
    if (session->eos_submitted != 0) {
        return frame_nvenc_ok(out_outcome, "End of stream was already submitted.");
    }
    if (session->initialized == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "End of stream cannot be signalled before initialization.");
    }
    (void)memset(&picture, 0, sizeof(picture));
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.encodePicFlags = (uint32_t)NV_ENC_PIC_FLAG_EOS;
    picture.completionEvent = session->eos_event;
    session->native_calls += 1u;
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_ENCODE);
    status = session->library->functions.nvEncEncodePicture(session->encoder, &picture);
    frame_nvenc_phase_end(session);
    if (status == NV_ENC_ERR_ENCODER_BUSY) {
        session->busy_results += 1u;
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_BUSY, (int32_t)status,
                                "NvEncEncodePicture(END_STREAM) reported ENCODER_BUSY.");
    }
    if (status != NV_ENC_SUCCESS) {
        const char *last = session->library->functions.nvEncGetLastErrorString(session->encoder);
        return frame_nvenc_fail(out_outcome, frame_nvenc_category(status), (int32_t)status,
                                "NvEncEncodePicture(END_STREAM) failed with NVENCSTATUS %d (%s).",
                                (int32_t)status, last != NULL ? last : "no detail");
    }
    session->eos_submitted = 1;
    return frame_nvenc_ok(out_outcome, "End of stream submitted.");
}

int32_t frame_nvenc_poll_eos(FrameNvencSession *session, uint32_t wait_ms, uint32_t *out_complete,
                             FrameNvencOutcome *out_outcome) {
    DWORD wait_result;

    frame_nvenc_clear(out_outcome);
    if (session == NULL || out_complete == NULL) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "Polling end of stream needs a session and a completion "
                                "out-parameter.");
    }
    *out_complete = 0u;
    if (session->eos_submitted == 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_INVALID, -1,
                                "End of stream was not submitted, so it cannot be polled.");
    }
    frame_nvenc_phase_begin(session, FRAME_NVENC_API_WAIT_EOS);
    wait_result = WaitForSingleObject((HANDLE)session->eos_event, (DWORD)wait_ms);
    frame_nvenc_phase_end(session);
    if (wait_result == WAIT_TIMEOUT) {
        return frame_nvenc_ok(out_outcome, "End of stream is not acknowledged yet.");
    }
    if (wait_result != WAIT_OBJECT_0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "Waiting for the end-of-stream event failed with Win32 result %lu.",
                                (unsigned long)wait_result);
    }
    *out_complete = 1u;
    return frame_nvenc_ok(out_outcome, "End of stream acknowledged.");
}

void frame_nvenc_session_stats(FrameNvencSession *session, uint32_t *out_native_calls,
                               uint32_t *out_busy_results, uint32_t *out_pending_inputs) {
    if (session == NULL) {
        return;
    }
    if (out_native_calls != NULL) {
        *out_native_calls = session->native_calls;
    }
    if (out_busy_results != NULL) {
        *out_busy_results = session->busy_results;
    }
    if (out_pending_inputs != NULL) {
        *out_pending_inputs = session->pending_inputs;
    }
}

int32_t frame_nvenc_session_close(FrameNvencSession *session, FrameNvencOutcome *out_outcome) {
    uint32_t slot;
    uint32_t index;
    NVENCSTATUS first_failure = NV_ENC_SUCCESS;
    const char *first_api = NULL;
    uint32_t cleanup_failures = 0u;
    int32_t stale_outputs = 0;
    int32_t stale_inputs = 0;
    int32_t unsafe_outputs = 0;
    int32_t unsafe_registrations = 0;
    int32_t destroy_failed = 0;
    uint64_t teardown_deadline = GetTickCount64() + (uint64_t)FRAME_NVENC_TEARDOWN_DEADLINE_MS;

    frame_nvenc_clear(out_outcome);
    if (session == NULL) {
        return FRAME_NVENC_OK;
    }

    /* 1. Establish that the driver is finished with every owned output before anything is
     * released. While any output cannot be settled, native use is unknown: nothing is
     * unregistered, destroyed, closed or freed, and the caller keeps the whole owning
     * graph (session, DLL reference, registrations, mappings, buffers, events, texture
     * backing and capture leases) instead of being told its resources were released. */
    for (slot = 0u; slot < FRAME_NVENC_OUTPUT_BUFFERS; ++slot) {
        if (session->output_in_use[slot] != 0) {
            stale_outputs += 1;
            if (!frame_nvenc_settle_output(session, slot, teardown_deadline, &first_failure, &first_api)) {
                unsafe_outputs += 1;
                continue;
            }
            {
                NVENCSTATUS status = frame_nvenc_release_output(session, slot, &cleanup_failures);
                if (status != NV_ENC_SUCCESS && first_failure == NV_ENC_SUCCESS) {
                    first_failure = status;
                    first_api = "NvEncUnmapInputResource";
                }
            }
            if (session->output_in_use[slot] != 0) {
                unsafe_outputs += 1;
            }
        }
    }
    if (unsafe_outputs != 0) {
        /* Ownership safety wins over every other reporting concern: the result must match
         * the resources that are actually still owned. A native-status failure observed
         * earlier is kept in the detail text, not allowed to replace the unsafe category. */
        return frame_nvenc_fail(
            out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, first_failure == NV_ENC_SUCCESS ? -1 : (int32_t)first_failure,
            "Native cleanup could not establish that the driver finished with %d owned "
            "output buffer(s) within the %u ms teardown deadline, so no registration, "
            "buffer, event, encoder or session resource was released. Completed outputs may "
            "have unmapped inputs; all unresolved handles remain recorded (cleanup status %d from %s).",
            unsafe_outputs, (uint32_t)FRAME_NVENC_TEARDOWN_DEADLINE_MS,
            (int32_t)first_failure, first_api != NULL ? first_api : "no call");
    }
    /* A failed submit can leave a mapping without an accepted output. Retain and
     * retry that mapping before unregistering its texture. */
    for (index = 0u; index < FRAME_NVENC_INPUT_REGISTRATIONS; ++index) {
        if (session->inputs[index].mapped != NULL) {
            NVENCSTATUS status;
            frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNMAP);
            status = session->library->functions.nvEncUnmapInputResource(session->encoder, session->inputs[index].mapped);
            frame_nvenc_phase_end(session);
            if (status != NV_ENC_SUCCESS) return frame_nvenc_fail(out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, status,
                "NvEncUnmapInputResource failed with NVENCSTATUS %d; the mapped input and dependencies remain owned.", status);
            session->inputs[index].mapped = NULL;
            session->pending_inputs = session->pending_inputs > 0u ? session->pending_inputs - 1u : 0u;
        }
    }
    /* 2. Unregister every remaining input registration. A failed unregister is a cleanup
     * failure and leaves the entry recorded, so it is never counted as a release. */
    for (index = 0u; index < FRAME_NVENC_INPUT_REGISTRATIONS; ++index) {
        if (session->inputs[index].used != 0) {
            NVENCSTATUS status;
            stale_inputs += 1;
            session->native_calls += 1u;
            frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNREGISTER_RESOURCE);
            status = session->library->functions.nvEncUnregisterResource(
                session->encoder, (NV_ENC_REGISTERED_PTR)session->inputs[index].registered);
            frame_nvenc_phase_end(session);
            if (status != NV_ENC_SUCCESS) {
                unsafe_registrations += 1;
                if (first_failure == NV_ENC_SUCCESS) {
                    first_failure = status;
                    first_api = "NvEncUnregisterResource";
                }
                continue;
            }
            session->inputs[index].used = 0;
            session->inputs[index].registered = NULL;
            session->inputs[index].texture = NULL;
        }
    }
    if (unsafe_registrations != 0) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, first_failure,
            "%s failed with NVENCSTATUS %d; unresolved registrations and their dependencies remain owned for retry.", first_api, first_failure);
    }
    /* Successful child releases are cleared exactly once. A failed release retains its
     * handle and every dependency; lower cleanup stages run only when this stage is safe. */
    for (slot = 0u; slot < FRAME_NVENC_OUTPUT_BUFFERS; ++slot) {
        if (session->outputs[slot] != NULL) {
            NVENCSTATUS status;
            session->native_calls += 1u;
            frame_nvenc_phase_begin(session, FRAME_NVENC_API_DESTROY_BITSTREAM);
            status = session->library->functions.nvEncDestroyBitstreamBuffer(session->encoder, session->outputs[slot]);
            frame_nvenc_phase_end(session);
            if (status == NV_ENC_SUCCESS) session->outputs[slot] = NULL;
            else if (first_failure == NV_ENC_SUCCESS) { first_failure = status; first_api = "NvEncDestroyBitstreamBuffer"; }
        }
    }
    if (first_failure != NV_ENC_SUCCESS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, first_failure,
            "%s failed with NVENCSTATUS %d; failed buffers and event/session dependencies remain owned for retry.", first_api, first_failure);
    }
    for (slot = 0u; slot <= FRAME_NVENC_OUTPUT_BUFFERS; ++slot) {
        void **event = slot == FRAME_NVENC_OUTPUT_BUFFERS ? &session->eos_event : &session->events[slot];
        int32_t *registered = slot == FRAME_NVENC_OUTPUT_BUFFERS ? &session->eos_registered : &session->event_registered[slot];
        if (*event != NULL) {
            NVENCSTATUS status = NV_ENC_SUCCESS;
            if (*registered != 0) {
                NV_ENC_EVENT_PARAMS registration;
                memset(&registration, 0, sizeof(registration));
                registration.version = NV_ENC_EVENT_PARAMS_VER;
                registration.completionEvent = *event;
                session->native_calls += 1u;
                frame_nvenc_phase_begin(session, FRAME_NVENC_API_UNREGISTER_EVENT);
                status = session->library->functions.nvEncUnregisterAsyncEvent(session->encoder, &registration);
                frame_nvenc_phase_end(session);
                if (status == NV_ENC_SUCCESS) *registered = 0;
                else if (first_failure == NV_ENC_SUCCESS) { first_failure = status; first_api = "NvEncUnregisterAsyncEvent"; }
            }
            if (*registered == 0) {
                if (CloseHandle((HANDLE)*event)) *event = NULL;
                else if (first_failure == NV_ENC_SUCCESS) { first_failure = NV_ENC_ERR_GENERIC; first_api = "CloseHandle(event)"; }
            }
        }
    }
    if (first_failure != NV_ENC_SUCCESS) {
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, first_failure,
            "%s failed with NVENCSTATUS %d; failed events and their session remain owned for retry.", first_api, first_failure);
    }
    /* 5. Destroy the encoder session itself. */
    if (session->encoder != NULL) {
        NVENCSTATUS status;
        session->native_calls += 1u;
        frame_nvenc_phase_begin(session, FRAME_NVENC_API_DESTROY);
        status = session->library->functions.nvEncDestroyEncoder(session->encoder);
        frame_nvenc_phase_end(session);
        if (status != NV_ENC_SUCCESS) {
            /* The session handle survived, so this is not confirmed destruction: the handle
             * and the session record stay reachable for a bounded retry, and the result must
             * say which children were released and which handle was retained. */
            destroy_failed = 1;
            if (first_failure == NV_ENC_SUCCESS) {
                first_failure = status;
                first_api = "NvEncDestroyEncoder";
            }
        } else {
            session->encoder = NULL;
            session->initialized = 0;
        }
    }
    if (destroy_failed != 0) {
        /* Children were released (outputs settled, mappings unmapped, registrations and
         * events released) but the encoder session itself survived, so the session record is
         * retained and this close is retryable and idempotent: a later call only repeats the
         * destroy step. This is never reported as confirmed destruction. */
        return frame_nvenc_fail(
            out_outcome, FRAME_NVENC_CLEANUP_UNSAFE, (int32_t)first_failure,
            "NvEncDestroyEncoder failed with NVENCSTATUS %d after %d output(s) and %d "
            "registration(s) were released; the encoder handle and this session record are "
            "retained for a bounded retry and no DLL or device ownership was dropped.",
            (int32_t)first_failure, stale_outputs, stale_inputs);
    }
    /* Reaching here means every output was settled and the encoder session was destroyed:
     * destruction is confirmed, so the session record can be freed. A cleanup error reported
     * below is a *confirmed* destruction with a failure, which is different from unknown
     * native use above. */
    frame_witness_release(session->witness_id);
    (void)free(session);

    if (first_failure != NV_ENC_SUCCESS) {
        return frame_nvenc_fail(out_outcome, frame_nvenc_category(first_failure),
                                (int32_t)first_failure,
                                "%s failed with NVENCSTATUS %d during cleanup (teardown also "
                                "released %d in-flight output buffer(s), %d registration(s) and %u "
                                "unmap failure(s)).",
                                first_api != NULL ? first_api : "A cleanup call",
                                (int32_t)first_failure, stale_outputs, stale_inputs,
                                cleanup_failures);
    }
    if (cleanup_failures != 0u) {
        /* A failed unmap means the driver may still reference that texture, so this is a
         * cleanup failure and not context. */
        return frame_nvenc_fail(out_outcome, FRAME_NVENC_FAILED, -1,
                                "Teardown released %d in-flight output buffer(s) and %d "
                                "registration(s), but %u input unmap(s) failed, so the driver may "
                                "still reference those textures.",
                                stale_outputs, stale_inputs, cleanup_failures);
    }
    if (stale_outputs != 0 || stale_inputs != 0) {
        /* Leftover in-flight work and registrations are normal on an early stop or a failing
         * session; they were released exactly once above and are reported as context rather
         * than a cleanup failure. */
        return frame_nvenc_ok(out_outcome,
                              "The encoder was destroyed with %d uncollected output buffer(s) and "
                              "%d stale registration(s); every resource was still released exactly "
                              "once.",
                              stale_outputs, stale_inputs);
    }
    return frame_nvenc_ok(out_outcome, "Encoder session destroyed and resources released.");
}
