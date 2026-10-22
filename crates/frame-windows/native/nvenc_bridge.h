/*
 * Frame-owned NVENC bridge ABI (FRAME-0104, ADR 0006).
 *
 * This header is the only encoder interface Rust declares. NVIDIA SDK structures,
 * bitfields, GUIDs, version macros and the function table stay inside the bridge's C
 * translation unit: nothing here is an SDK layout, a COM type or a raw video type, and
 * every buffer that crosses the ABI carries a checked length. The bridge never lets a
 * language exception (C++, SEH or a Rust panic) propagate across this boundary; every
 * entry point returns an int32_t status and fills a bounded FrameNvencOutcome.
 */
#ifndef FRAME_NVENC_BRIDGE_H
#define FRAME_NVENC_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Bounded text carried with every outcome. Always NUL-terminated. */
#define FRAME_NVENC_DETAIL_BYTES 512u
/* Bounded initialization bytes (SPS/PPS); ADR 0006 caps codec configuration at 64 KiB. */
#define FRAME_NVENC_CODEC_CONFIG_BYTES 1024u
/* ADR 0006: four native output buffers/events, plus a separate EOS event. */
#define FRAME_NVENC_OUTPUT_BUFFERS 4u
/* ADR 0006: private registration is bounded to four textures from one capture pool. */
#define FRAME_NVENC_INPUT_REGISTRATIONS 4u
/* ADR 0006: at most 8 MiB per access unit. */
#define FRAME_NVENC_MAX_ACCESS_UNIT_BYTES (8u * 1024u * 1024u)
/* Bounded wait for one owned output before teardown may release it. */
#define FRAME_NVENC_SETTLE_POLL_MS 5u
/* Explicit overall teardown deadline; expiring it leaves native use unknown. */
#define FRAME_NVENC_TEARDOWN_DEADLINE_MS 5000u
/* One page of headroom, so a genuinely maximal access unit is still distinguishable
 * from a driver-truncated one instead of being silently accepted. */
#define FRAME_NVENC_ACCESS_UNIT_HEADROOM_BYTES (64u * 1024u)

/* Codec selection is explicit; zero preserves every existing H.264 request. */
#define FRAME_NVENC_CODEC_H264 0u
#define FRAME_NVENC_CODEC_HEVC_MAIN10 1u
#define FRAME_NVENC_CODEC_AV1 2u
#define FRAME_NVENC_CODEC_AV1_MAIN10 3u

/* Zero retains the packed RGB path. YUV inputs bypass RGB conversion. */
#define FRAME_NVENC_INPUT_RGB 0u
#define FRAME_NVENC_INPUT_NV12 1u
#define FRAME_NVENC_INPUT_P010 2u

/*
 * Frame-owned outcome categories. The native NVENCSTATUS is preserved separately in
 * FrameNvencOutcome::status, so a caller can report exactly which API call failed with
 * which status instead of a collapsed "encoder error".
 */
enum {
    FRAME_NVENC_OK = 0,
    /* No NVIDIA driver/DLL/device, or the requested capability is absent. */
    FRAME_NVENC_UNAVAILABLE = 1,
    /* The installed driver is older than the API this bridge was compiled against. */
    FRAME_NVENC_VERSION = 2,
    /* A documented unsupported parameter, geometry or format. */
    FRAME_NVENC_UNSUPPORTED = 3,
    /* Frame passed something invalid: a programming error, not a device condition. */
    FRAME_NVENC_INVALID = 4,
    /* Transient: NEED_MORE_INPUT or an encoder-busy result. Retry, never a failure. */
    FRAME_NVENC_BUSY = 5,
    /* Any other native failure, including out of memory and device loss. */
    FRAME_NVENC_FAILED = 6,
    /* Cleanup could not establish that the driver stopped using the resources. */
    FRAME_NVENC_CLEANUP_UNSAFE = 7
};

typedef struct {
    /* One of the FRAME_NVENC_* categories above. */
    int32_t category;
    /* The native NVENCSTATUS that produced the outcome, or -1 when no native call did. */
    int32_t status;
    /* Bounded, human-readable detail naming the API call and its arguments. */
    char detail[FRAME_NVENC_DETAIL_BYTES];
} FrameNvencOutcome;

/*
 * The rate-control modes this bridge can apply (FRAME-0207). The caller's policy decides
 * which one a recording uses; the bridge only applies it and reports it back, and it
 * refuses a mode the driver did not report rather than substituting another one.
 */
enum {
    FRAME_NVENC_RATE_CONTROL_CBR = 0,
    FRAME_NVENC_RATE_CONTROL_VBR = 1,
    FRAME_NVENC_RATE_CONTROL_LOSSLESS = 2,
    FRAME_NVENC_RATE_CONTROL_CQP = 3
};

/*
 * The preset/tuning pairs this bridge can apply (FRAME-0207). Every pair is requested from
 * the driver and verified before it initializes, so a preset the installed driver does not
 * offer is an explicit failure and never a silent substitution.
 */
enum {
    /* P1 with ultra-low-latency tuning: the existing default. */
    FRAME_NVENC_PRESET_LOW_LATENCY = 0,
    /* P5 with high-quality tuning. */
    FRAME_NVENC_PRESET_HIGH_QUALITY = 1,
    /* P7 with high-quality tuning. */
    FRAME_NVENC_PRESET_MAXIMUM_QUALITY = 2,
    FRAME_NVENC_PRESET_LOSSLESS = 3
};

/* Opaque, process-wide, reference-counted view of the system NVENC library. */
typedef struct FrameNvencLibrary FrameNvencLibrary;

typedef struct {
    /* Driver's largest supported API version, in NVIDIA's own max-version encoding:
     * the low four bits are the minor version, the remaining bits the major version. */
    uint32_t api_version_max;
    uint32_t driver_major;
    uint32_t driver_minor;
    /* NVENCAPI_VERSION this bridge was compiled against (NVIDIA's struct encoding). */
    uint32_t api_version_required;
} FrameNvencLibraryInfo;

/*
 * Loads the system nvEncodeAPI64.dll and resolves the API. References are counted, so
 * the DLL stays loaded until the last release; every successful acquire must be
 * matched by exactly one frame_nvenc_library_release.
 */
int32_t frame_nvenc_library_acquire(FrameNvencLibrary **out_library,
                                    FrameNvencLibraryInfo *out_info,
                                    FrameNvencOutcome *out_outcome);
void frame_nvenc_library_release(FrameNvencLibrary *library);

/* Accepted initialization snapshot plus queried capabilities and sequence headers.
 * NVENC exposes no get-current-rate-control configuration function. */
typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t rate_num;
    uint32_t rate_den;
    uint32_t bitrate_bps;
    /* The applied peak bitrate: equal to bitrate_bps for CBR. */
    uint32_t max_bitrate_bps;
    uint32_t gop_length;
    uint32_t idr_period;
    uint32_t frame_interval_p;
    uint32_t enable_ptd;
    uint32_t enable_async;
    uint32_t repeat_spspps;
    uint32_t level;
    /* 0 = codec-selected profile, 1 = H.264 High. */
    uint32_t profile_high;
    /* 0 = limited (studio) range signalled in the VUI, 1 = full range. */
    uint32_t vui_full_range;
    /* The FRAME_NVENC_RATE_CONTROL_* mode and the FRAME_NVENC_PRESET_* pair applied. */
    uint32_t rate_control_mode;
    uint32_t preset;
    /* Capability answers read from the driver before initialization. */
    uint32_t caps_async_encode;
    uint32_t caps_width_max;
    uint32_t caps_height_max;
    uint32_t caps_rate_control_modes;
    uint32_t caps_h264_guid;
    uint32_t caps_input_argb;
    uint32_t caps_profile_high;
    /* 1 when the requested preset/tuning pair was obtained from the driver. */
    uint32_t caps_preset_available;
    /* Zero for a qualification probe, which allocates no native output buffers. */
    uint32_t output_buffers;
    uint32_t output_buffer_capacity;
    uint32_t max_access_unit_bytes;
    /* Codec-defined initialization bytes (Annex-B SPS/PPS) read back from the session. */
    uint32_t codec_config_bytes;
    uint8_t codec_config[FRAME_NVENC_CODEC_CONFIG_BYTES];
    uint32_t codec;
    uint32_t caps_10bit;
    uint32_t caps_input_abgr10;
    uint32_t input_format;
    uint32_t caps_input_formats;
    uint32_t caps_profile;
    /* Accepted initialization snapshot; no NVENC get-current-config API exists. */
    uint32_t advanced[9];
} FrameNvencSettings;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t rate_num;
    uint32_t rate_den;
    uint32_t bitrate_bps;
    /* The peak bitrate the mode may use; equal to bitrate_bps for CBR. */
    uint32_t max_bitrate_bps;
    uint32_t gop_length;
    uint32_t rate_control_mode;
    uint32_t preset;
    uint32_t codec;
    uint32_t input_format;
    /* Zero means the legacy BT.709 SDR / BT.2020 PQ Main10 signal. */
    uint32_t colour_present;
    uint32_t primaries;
    uint32_t transfer;
    uint32_t matrix;
    uint32_t full_range;
    uint32_t content_light_present;
    uint32_t max_cll;
    uint32_t max_fall;
    uint32_t mastering_present;
    /* R,G,B,white x/y: HEVC 1/50000, AV1 1/65536. Luminance:
     * HEVC 1/10000 nit, AV1 max 1/256 and min 1/16384 nit. */
    uint32_t chromaticity[8];
    uint32_t luminance_max;
    uint32_t luminance_min;
    /* Zero inherits policy; preset=1..7, tuning=1 HQ/2 LL/3 ULL,
     * multipass=1 off/2 quarter/3 full, AQ=1 off/2 on. */
    uint32_t advanced_preset;
    uint32_t advanced_tuning;
    uint32_t advanced_qp;
    uint32_t advanced_multipass;
    uint32_t advanced_spatial_aq;
    uint32_t advanced_temporal_aq;
} FrameNvencSessionRequest;

/*
 * Qualification: opens a real session on the given D3D11 device for exactly this request,
 * applies it, verifies capabilities and format support first, reads SPS/PPS and destroys
 * every resource before returning. Vendor presence alone never produces a success.
 */
int32_t frame_nvenc_probe(FrameNvencLibrary *library, void *d3d11_device,
                          const FrameNvencSessionRequest *request,
                          FrameNvencSettings *out_settings, FrameNvencOutcome *out_outcome);

/* One initialized hardware encoder session. Owned and used by exactly one thread. */
typedef struct FrameNvencSession FrameNvencSession;

int32_t frame_nvenc_session_open(FrameNvencLibrary *library, void *d3d11_device,
                                 const FrameNvencSessionRequest *request,
                                 FrameNvencSession **out_session,
                                 FrameNvencSettings *out_settings,
                                 FrameNvencOutcome *out_outcome);

/* Registers one capture texture. Bounded to FRAME_NVENC_INPUT_REGISTRATIONS textures. */
int32_t frame_nvenc_register_input(FrameNvencSession *session, void *d3d11_texture,
                                   uint32_t width, uint32_t height,
                                   uint32_t *out_registration, FrameNvencOutcome *out_outcome);

int32_t frame_nvenc_unregister_input(FrameNvencSession *session, uint32_t registration,
                                     FrameNvencOutcome *out_outcome);

/*
 * Submits one picture asynchronously. The registered texture is mapped for the
 * submission and stays mapped until frame_nvenc_collect completes that output buffer;
 * the caller must pass the same slot back to frame_nvenc_collect.
 *
 * `out_buffered` distinguishes the SDK's NEED_MORE_INPUT result, which is an accepted
 * picture the driver buffers: the input stays mapped, the output slot and its event stay
 * owned, and the access unit arrives with a later submission or with end of stream.
 * Either way the caller keeps the slot reserved until its event signals. ENCODER_BUSY is
 * returned as FRAME_NVENC_BUSY with no slot and no accepted input.
 */
int32_t frame_nvenc_submit(FrameNvencSession *session, uint32_t registration, uint64_t pts_ns,
                           uint64_t duration_ns, uint32_t force_idr, uint32_t *out_slot,
                           uint32_t *out_buffered, FrameNvencOutcome *out_outcome);

enum {
    FRAME_NVENC_STATE_PENDING = 0,
    FRAME_NVENC_STATE_READY = 1
};

typedef struct {
    uint32_t state;
    /* Access-unit size in bytes when state is READY. */
    uint32_t bytes;
    /* Native output timestamp and duration, echoed back from the locked bitstream. */
    uint64_t pts_ns;
    uint64_t duration_ns;
    uint64_t frame_index;
    /* Native NV_ENC_PIC_TYPE value; keyframe is a native IDR flag, never guessed. */
    uint32_t picture_type;
    uint32_t keyframe;
    uint32_t average_qp;
} FrameNvencPacketInfo;

/*
 * Waits up to wait_ms for one submitted picture, then copies its complete Annex-B
 * access unit into destination. A unit larger than destination_capacity is an explicit
 * failure: a bitstream is never truncated, partially copied or accepted. On READY and
 * on every failure the input mapping and the output slot are released exactly once.
 */
int32_t frame_nvenc_collect(FrameNvencSession *session, uint32_t slot, uint32_t wait_ms,
                            uint8_t *destination, uint32_t destination_capacity,
                            FrameNvencPacketInfo *out_info, FrameNvencOutcome *out_outcome);

/* Idempotent EOS: closes the input stream and signals the separate EOS event. */
int32_t frame_nvenc_begin_eos(FrameNvencSession *session, FrameNvencOutcome *out_outcome);
/* Waits up to wait_ms for EOS acknowledgement; complete is 1 only when it signalled. */
int32_t frame_nvenc_poll_eos(FrameNvencSession *session, uint32_t wait_ms, uint32_t *out_complete,
                             FrameNvencOutcome *out_outcome);

/* Native call and retry counters, for diagnostics only. */
void frame_nvenc_session_stats(FrameNvencSession *session, uint32_t *out_native_calls,
                               uint32_t *out_busy_results, uint32_t *out_pending_inputs);

/*
 * Bounded call-phase record: the NVENC API this session is inside right now, when it
 * started (QueryPerformanceCounter) and on which thread, plus the count of completed
 * calls. Written only by the session's own thread and read by a diagnostics thread, with
 * no lock held across a native call, so a call that never returns is named exactly
 * instead of being inferred from unchanged counters. It never records pixel data.
 */
typedef struct {
    char api[64];
    uint64_t session_id;
    uint64_t started_qpc;
    uint64_t ended_qpc;
    uint64_t observed_qpc;
    uint64_t ticks_per_second;
    uint32_t thread_id;
    uint32_t running;
    uint32_t completed_calls;
    uint32_t reserved;
} FrameNvencPhase;

void frame_nvenc_session_phase(FrameNvencSession *session, FrameNvencPhase *out_phase);

/* 64 session-owned slots, immutable copies for observers. UINT32_MAX means a
 * contended snapshot (unknown), not idle. No lock spans a driver call. */
uint64_t frame_witness_create(const char *initial_phase);
void frame_witness_release(uint64_t id);
void frame_witness_begin(uint64_t id, const char *api);
void frame_witness_end(uint64_t id);
uint32_t frame_witness_snapshots(FrameNvencPhase *records, uint32_t capacity);

/*
 * Releases every native resource exactly once, in the order the SDK requires (mapped
 * inputs, registrations, bitstream buffers, async events, encoder, handles) and frees
 * the session only after confirmed release. CLEANUP_UNSAFE preserves failed handles
 * and their dependencies for owner-thread retry; it never implies safe recycling.
 */
int32_t frame_nvenc_session_close(FrameNvencSession *session, FrameNvencOutcome *out_outcome);

#ifdef __cplusplus
}
#endif

#endif /* FRAME_NVENC_BRIDGE_H */
