//! The native NVENC bridge binding and the probe device path (FRAME-0104, ADR 0006).
//!
//! This module is the reviewed FFI boundary of the encoder path, and the only place in
//! the crate that performs an encoder `unsafe` call. It declares the Frame-owned ABI
//! from `native/nvenc_bridge.h` — opaque session handles and POD values only — and
//! never an NVIDIA SDK structure, bitfield, GUID, version macro or function table:
//! those exist only in the C bridge, which is compiled by `build.rs` against the pinned
//! vendored header.
//!
//! It also contains the only encoder-side Direct3D device creation, used by the
//! qualification probe. Production encoding always uses the capture lease's own device
//! through [`NativeDriver::open`]; the probe device is temporary and is destroyed with
//! the qualification session.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::Arc;

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1, IDXGIDevice,
    IDXGIFactory1,
};
use windows::core::Interface;

use super::driver::{
    EncodeTexture, NativeCollect, NativeEos, NativePacket, NativeStats, NativeSubmit, NvencDriver,
};
use super::hdr_convert::HdrConverter;
use super::{
    MAX_ACCESS_UNIT_BYTES, MAX_CODEC_CONFIG_BYTES, MAX_SCANNED_ENCODE_ADAPTERS,
    NativeEncoderConfig, NativeEncoderPreset, NativeEncoderSettings, NativeRateControl,
    internal_error,
};

/// NVIDIA's PCI vendor identifier in `DXGI_ADAPTER_DESC1`.
const NVIDIA_VENDOR_ID: u32 = 0x10DE;
/// Bounded detail text the bridge copies with every outcome.
const DETAIL_BYTES: usize = 512;
/// The driver's reusable collection buffer. It is charged permanently against the owned
/// packet-memory bound, so a completed access unit and this buffer together can never
/// exceed the declared total.
pub(crate) const SCRATCH_BYTES: u64 = MAX_ACCESS_UNIT_BYTES as u64;
/// Bounded initialization bytes (SPS/PPS) the bridge reports.
const CODEC_CONFIG_BYTES: usize = 1024;

/// Opaque bridge objects. Rust never sees their layout.
#[repr(C)]
struct FrameNvencLibrary {
    _opaque: [u8; 0],
}

#[repr(C)]
struct FrameNvencSession {
    _opaque: [u8; 0],
}

/// Bridge outcome: a Frame-owned category, the native status and bounded detail text.
#[repr(C)]
struct FrameNvencOutcome {
    category: i32,
    status: i32,
    detail: [c_char; DETAIL_BYTES],
}

#[repr(C)]
struct FrameNvencLibraryInfo {
    api_version_max: u32,
    driver_major: u32,
    driver_minor: u32,
    api_version_required: u32,
}

/// The requested configuration as the driver reported it after initialization.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrameNvencSettings {
    width: u32,
    height: u32,
    rate_num: u32,
    rate_den: u32,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    gop_length: u32,
    idr_period: u32,
    frame_interval_p: u32,
    enable_ptd: u32,
    enable_async: u32,
    repeat_spspps: u32,
    level: u32,
    profile_high: u32,
    vui_full_range: u32,
    rate_control_mode: u32,
    preset: u32,
    caps_async_encode: u32,
    caps_width_max: u32,
    caps_height_max: u32,
    caps_rate_control_modes: u32,
    caps_h264_guid: u32,
    caps_input_argb: u32,
    caps_profile_high: u32,
    caps_preset_available: u32,
    output_buffers: u32,
    output_buffer_capacity: u32,
    max_access_unit_bytes: u32,
    codec_config_bytes: u32,
    codec_config: [u8; CODEC_CONFIG_BYTES],
    codec: u32,
    caps_10bit: u32,
    caps_input_abgr10: u32,
    input_format: u32,
    caps_input_formats: u32,
    caps_profile: u32,
    advanced: super::NativeAdvancedSettings,
}

use crate::witness::FrameNvencPhase;
pub(crate) fn global_phase() -> String {
    crate::witness::describe_all()
}

#[repr(C)]
struct FrameNvencSessionRequest {
    width: u32,
    height: u32,
    rate_num: u32,
    rate_den: u32,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    gop_length: u32,
    rate_control_mode: u32,
    preset: u32,
    codec: u32,
    input_format: u32,
    colour_present: u32,
    primaries: u32,
    transfer: u32,
    matrix: u32,
    full_range: u32,
    content_light_present: u32,
    max_cll: u32,
    max_fall: u32,
    mastering_present: u32,
    chromaticity: [u32; 8],
    luminance_max: u32,
    luminance_min: u32,
    advanced_preset: u32,
    advanced_tuning: u32,
    advanced_qp: u32,
    advanced_multipass: u32,
    advanced_spatial_aq: u32,
    advanced_temporal_aq: u32,
}

impl FrameNvencSessionRequest {
    /// The bridge request for one Frame-owned configuration. The numeric mode and preset
    /// values are the Frame-owned ABI constants declared in `nvenc_bridge.h`.
    fn from_config(config: &NativeEncoderConfig) -> Self {
        Self {
            width: config.width,
            height: config.height,
            rate_num: config.rate_num.get(),
            rate_den: config.rate_den.get(),
            bitrate_bps: config.bitrate_bps,
            max_bitrate_bps: config.peak_bitrate_bps,
            gop_length: config.gop_frames,
            rate_control_mode: config.rate_control.bridge_value(),
            preset: config.preset.bridge_value(),
            codec: 0,
            input_format: 0,
            colour_present: 0,
            primaries: 0,
            transfer: 0,
            matrix: 0,
            full_range: 0,
            content_light_present: 0,
            max_cll: 0,
            max_fall: 0,
            mastering_present: 0,
            chromaticity: [0; 8],
            luminance_max: 0,
            luminance_min: 0,
            advanced_preset: config.encoder_overrides.preset.map_or(0, |v| v as u32 + 1),
            advanced_tuning: config.encoder_overrides.tuning.map_or(0, |v| match v {
                frame_contracts::VideoEncoderTuning::HighQuality => 1,
                frame_contracts::VideoEncoderTuning::LowLatency => 2,
                frame_contracts::VideoEncoderTuning::UltraLowLatency => 3,
            }),
            advanced_qp: config.encoder_overrides.qp.unwrap_or(0),
            advanced_multipass: config
                .encoder_overrides
                .multipass
                .map_or(0, |v| v as u32 + 1),
            advanced_spatial_aq: config
                .encoder_overrides
                .spatial_aq
                .map_or(0, |v| if v { 2 } else { 1 }),
            advanced_temporal_aq: config
                .encoder_overrides
                .temporal_aq
                .map_or(0, |v| if v { 2 } else { 1 }),
        }
    }

    fn with_colour(mut self, colour: frame_encode::VideoColour) -> Self {
        self.colour_present = 1;
        self.primaries = colour.primaries.into();
        self.transfer = colour.transfer_characteristics.into();
        self.matrix = colour.matrix_coefficients.into();
        self.full_range = u32::from(colour.range == 2);
        if let Some(light) = colour.content_light {
            self.content_light_present = 1;
            self.max_cll = light.max_cll.into();
            self.max_fall = light.max_fall.into();
        }
        if let Some(m) = colour.mastering {
            self.mastering_present = 1;
            let av1 = self.codec == 2 || self.codec == 3;
            let chroma_scale = if av1 { 65536.0 } else { 50000.0 };
            self.chromaticity = [
                m.red.0, m.red.1, m.green.0, m.green.1, m.blue.0, m.blue.1, m.white.0, m.white.1,
            ]
            .map(|v| ((v * chroma_scale).round() as u32).min(u16::MAX.into()));
            self.luminance_max =
                (m.luminance_max * if av1 { 256.0 } else { 10000.0 }).round() as u32;
            self.luminance_min =
                (m.luminance_min * if av1 { 16384.0 } else { 10000.0 }).round() as u32;
        }
        self
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameNvencPacketInfo {
    state: u32,
    bytes: u32,
    pts_ns: u64,
    duration_ns: u64,
    frame_index: u64,
    picture_type: u32,
    keyframe: u32,
    average_qp: u32,
}

/// Bridge categories. The native status is preserved beside them in the message.
const CATEGORY_OK: i32 = 0;
const CATEGORY_UNAVAILABLE: i32 = 1;
const CATEGORY_VERSION: i32 = 2;
const CATEGORY_UNSUPPORTED: i32 = 3;
const CATEGORY_INVALID: i32 = 4;
const CATEGORY_BUSY: i32 = 5;
const CATEGORY_FAILED: i32 = 6;
/// Cleanup could not establish that the driver stopped using the resources.
const CATEGORY_CLEANUP_UNSAFE: i32 = 7;

/// Bridge collection states.
const STATE_PENDING: u32 = 0;
const STATE_READY: u32 = 1;

unsafe extern "C" {
    fn frame_nvenc_library_acquire(
        out_library: *mut *mut FrameNvencLibrary,
        out_info: *mut FrameNvencLibraryInfo,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_library_release(library: *mut FrameNvencLibrary);
    fn frame_nvenc_probe(
        library: *mut FrameNvencLibrary,
        d3d11_device: *mut c_void,
        request: *const FrameNvencSessionRequest,
        out_settings: *mut FrameNvencSettings,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_session_open(
        library: *mut FrameNvencLibrary,
        d3d11_device: *mut c_void,
        request: *const FrameNvencSessionRequest,
        out_session: *mut *mut FrameNvencSession,
        out_settings: *mut FrameNvencSettings,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_register_input(
        session: *mut FrameNvencSession,
        d3d11_texture: *mut c_void,
        width: u32,
        height: u32,
        out_registration: *mut u32,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_unregister_input(
        session: *mut FrameNvencSession,
        registration: u32,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_submit(
        session: *mut FrameNvencSession,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: u32,
        out_slot: *mut u32,
        out_buffered: *mut u32,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_collect(
        session: *mut FrameNvencSession,
        slot: u32,
        wait_ms: u32,
        destination: *mut u8,
        destination_capacity: u32,
        out_info: *mut FrameNvencPacketInfo,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_begin_eos(
        session: *mut FrameNvencSession,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_poll_eos(
        session: *mut FrameNvencSession,
        wait_ms: u32,
        out_complete: *mut u32,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_session_stats(
        session: *mut FrameNvencSession,
        out_native_calls: *mut u32,
        out_busy_results: *mut u32,
        out_pending_inputs: *mut u32,
    );
    fn frame_nvenc_session_close(
        session: *mut FrameNvencSession,
        out_outcome: *mut FrameNvencOutcome,
    ) -> i32;
    fn frame_nvenc_session_phase(session: *mut FrameNvencSession, out_phase: *mut FrameNvencPhase);
}

/// A bounded, always-NUL-terminated bridge outcome as owned Rust text.
fn outcome_detail(outcome: &FrameNvencOutcome) -> String {
    // SAFETY: the bridge always NUL-terminates `detail` inside its bounded array, so the
    // pointer is valid and the string is terminated within DETAIL_BYTES.
    let text = unsafe { CStr::from_ptr(outcome.detail.as_ptr()) };
    text.to_string_lossy().into_owned()
}

fn empty_outcome() -> FrameNvencOutcome {
    FrameNvencOutcome {
        category: CATEGORY_OK,
        status: -1,
        detail: [0; DETAIL_BYTES],
    }
}

/// Maps one bridge outcome onto the accepted error taxonomy.
///
/// Only the bridge's Frame-owned category is interpreted here: no NVIDIA status value
/// is transcribed into Rust. The native status and the failing API stay in the message.
fn outcome_error(
    outcome: &FrameNvencOutcome,
    operation: &str,
    session_started: bool,
) -> RecorderError {
    let detail = outcome_detail(outcome);
    let recovery = if session_started {
        RecoveryStatus::Unknown
    } else {
        RecoveryStatus::NoRecording
    };
    let action = if session_started {
        "The encoded stream is incomplete: discard the artifact and retry the encode."
    } else {
        "No stream was produced; fix the reported hardware or configuration condition and retry."
    };
    let code = match outcome.category {
        CATEGORY_UNAVAILABLE | CATEGORY_VERSION => ErrorCode::EncoderUnavailable,
        CATEGORY_UNSUPPORTED => ErrorCode::UnsupportedCodec,
        CATEGORY_INVALID | CATEGORY_FAILED => ErrorCode::Internal,
        _ => ErrorCode::Internal,
    };
    RecorderError {
        code,
        message: format!(
            "{operation} failed: {detail} (bridge category {}, native NVENCSTATUS {}).",
            outcome.category, outcome.status
        ),
        recovery,
        action: action.to_string(),
    }
}

/// True when the bridge reported a transient result the caller must retry.
fn is_busy(category: i32) -> bool {
    category == CATEGORY_BUSY
}

/// Loads the system NVENC library once and keeps it loaded while any handle exists.
pub(crate) struct NativeLibrary {
    handle: *mut FrameNvencLibrary,
}

// SAFETY: the bridge handle is a process-wide, reference-counted pointer with no thread
// affinity; the C bridge serializes its own load/release with a lock, and every session
// that uses it is used by exactly one thread.
unsafe impl Send for NativeLibrary {}
// SAFETY: see the `Send` justification; the handle carries no interior Rust state.
unsafe impl Sync for NativeLibrary {}

impl NativeLibrary {
    /// Loads `nvEncodeAPI64.dll` from the Windows system directory and negotiates the
    /// API version. A missing DLL, an old driver or an incomplete function table is an
    /// explicit `EncoderUnavailable` error.
    pub(crate) fn acquire() -> Result<(Arc<Self>, NativeLibraryInfo), RecorderError> {
        let mut handle: *mut FrameNvencLibrary = ptr::null_mut();
        let mut info = FrameNvencLibraryInfo {
            api_version_max: 0,
            driver_major: 0,
            driver_minor: 0,
            api_version_required: 0,
        };
        let mut outcome = empty_outcome();
        // SAFETY: both out-parameters are valid, initialized stack values and the bridge
        // only writes into them.
        let category = unsafe { frame_nvenc_library_acquire(&mut handle, &mut info, &mut outcome) };
        if category != CATEGORY_OK || handle.is_null() {
            return Err(outcome_error(
                &outcome,
                "Loading the system NVENC library",
                false,
            ));
        }
        let negotiated = NativeLibraryInfo {
            api_version_max: info.api_version_max,
            driver_major: info.driver_major,
            driver_minor: info.driver_minor,
            api_version_required: info.api_version_required,
        };
        Ok((Arc::new(Self { handle }), negotiated))
    }
}

/// Negotiated API versions, reported for diagnostics only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeLibraryInfo {
    pub(crate) api_version_max: u32,
    pub(crate) driver_major: u32,
    pub(crate) driver_minor: u32,
    pub(crate) api_version_required: u32,
}

impl NativeLibraryInfo {
    /// Human-readable library and driver version text, for diagnostics only.
    pub(crate) fn describe(&self) -> String {
        format!(
            "nvEncodeAPI64.dll from the Windows system directory; driver API {}.{} (NVIDIA max-version value {}), bridge API major {} (NVENCAPI_VERSION {})",
            self.driver_major,
            self.driver_minor,
            self.api_version_max,
            self.api_version_required & 0xFF,
            self.api_version_required
        )
    }
}

impl Drop for NativeLibrary {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: balances exactly one successful frame_nvenc_library_acquire.
            unsafe { frame_nvenc_library_release(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

/// Converts the bridge's reported configuration into the Frame-owned settings type.
fn settings_from_bridge(raw: &FrameNvencSettings) -> Result<NativeEncoderSettings, RecorderError> {
    let length = usize::try_from(raw.codec_config_bytes).unwrap_or(usize::MAX);
    if length == 0 || length > CODEC_CONFIG_BYTES {
        return Err(internal_error(format!(
            "The native encoder reported {length} initialization bytes, which is outside the bounded 1..={CODEC_CONFIG_BYTES} range."
        )));
    }
    let config = raw.codec_config.get(..length).ok_or_else(|| {
        internal_error(
            "The native encoder reported more initialization bytes than its bounded buffer."
                .to_string(),
        )
    })?;
    if config.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(internal_error(format!(
            "The native encoder reported {} initialization bytes, above the accepted {MAX_CODEC_CONFIG_BYTES}-byte limit.",
            config.len()
        )));
    }
    Ok(NativeEncoderSettings {
        advanced: raw.advanced,
        width: raw.width,
        height: raw.height,
        rate_num: raw.rate_num,
        rate_den: raw.rate_den,
        bitrate_bps: raw.bitrate_bps,
        max_bitrate_bps: raw.max_bitrate_bps,
        gop_length: raw.gop_length,
        idr_period: raw.idr_period,
        frame_interval_p: raw.frame_interval_p,
        enable_ptd: raw.enable_ptd == 1,
        enable_async: raw.enable_async == 1,
        repeat_spspps: raw.repeat_spspps == 1,
        level: raw.level,
        profile_high: raw.profile_high == 1,
        vui_full_range: raw.vui_full_range == 1,
        rate_control: NativeRateControl::from_bridge(raw.rate_control_mode)?,
        preset: NativeEncoderPreset::from_bridge(raw.preset)?,
        caps_async_encode: raw.caps_async_encode == 1,
        caps_width_max: raw.caps_width_max,
        caps_height_max: raw.caps_height_max,
        caps_rate_control_modes: raw.caps_rate_control_modes,
        caps_h264_guid: raw.caps_h264_guid == 1,
        caps_input_argb: raw.caps_input_argb == 1,
        caps_profile_high: raw.caps_profile_high == 1,
        caps_preset_available: raw.caps_preset_available == 1,
        output_buffers: raw.output_buffers,
        output_buffer_capacity: raw.output_buffer_capacity,
        max_access_unit_bytes: raw.max_access_unit_bytes,
        codec_config: config.to_vec(),
        codec: raw.codec,
        input_format: raw.input_format,
        caps_input_formats: raw.caps_input_formats,
        caps_profile: raw.caps_profile == 1,
        caps_10bit: raw.caps_10bit == 1,
    })
}

/// The production driver: one bridge session owned by one worker thread.
pub(crate) struct NativeDriver {
    library: Arc<NativeLibrary>,
    session: *mut FrameNvencSession,
    /// Set when the bridge reported that it could not establish that the driver finished
    /// with some resources: the session handle, its mappings, buffers and events are then
    /// deliberately kept reachable and owned, and this driver must not be dropped as if
    /// cleanup had succeeded.
    still_owned: bool,
    settings: NativeEncoderSettings,
    stats: NativeStats,
    /// Reused collection buffer, allocated once at open. Every byte it can hold was
    /// already reserved as an access-unit credit by the submission whose output is being
    /// collected, so it adds no buffer beyond the accepted bound and is never a fifth
    /// owned surface.
    scratch: Vec<u8>,
    hdr_converter: Option<HdrConverter>,
    colour: Option<frame_encode::VideoColour>,
}

// SAFETY: the bridge session is owned by exactly one worker thread from creation until
// `close`; it carries no thread affinity of its own and the bridge is lock-free per
// session, so moving it to the worker that will use it is sound. It is never shared.
unsafe impl Send for NativeDriver {}

impl NativeDriver {
    /// Opens a real hardware session for exactly `config` on the capture device behind
    /// `device`. This is the "initialize the selected capture device again for the
    /// requested dimensions and rate" step of ADR 0006.
    pub(crate) fn open(
        device: *mut c_void,
        config: &NativeEncoderConfig,
    ) -> Result<Self, RecorderError> {
        Self::open_codec(device, config, 0)
    }

    fn open_codec(
        device: *mut c_void,
        config: &NativeEncoderConfig,
        codec: u32,
    ) -> Result<Self, RecorderError> {
        Self::open_signal(device, config, codec, 0, None, codec == 1 || codec == 3)
    }

    pub(crate) fn open_signal(
        device: *mut c_void,
        config: &NativeEncoderConfig,
        codec: u32,
        input_format: u32,
        colour: Option<frame_encode::VideoColour>,
        convert_scrgb: bool,
    ) -> Result<Self, RecorderError> {
        if device.is_null() {
            return Err(internal_error(
                "The capture lease did not expose a Direct3D device for the encoder.".to_string(),
            ));
        }
        let library = NativeLibrary::acquire()?.0;
        let main10 = codec == 1 || codec == 3;
        config.validate()?;
        if !config.encoder_overrides.is_empty() && (codec != 0 && codec != 2) {
            return Err(internal_error(
                "Native Advanced overrides are qualified only for SDR H.264 and AV1.".into(),
            ));
        }
        let mut request = FrameNvencSessionRequest::from_config(config);
        request.codec = codec;
        request.input_format = input_format;
        if let Some(colour) = colour {
            colour.validate()?;
            request = request.with_colour(colour);
        }
        let hdr_converter = if convert_scrgb {
            Some(HdrConverter::new(device)?)
        } else {
            None
        };
        let mut session: *mut FrameNvencSession = ptr::null_mut();
        let mut raw = FrameNvencSettings::zeroed();
        let mut outcome = empty_outcome();
        // SAFETY: the library handle is alive for this call, the device pointer comes
        // from the live capture payload retained by the caller's lease, and both
        // out-parameters are valid initialized stack values.
        let category = unsafe {
            frame_nvenc_session_open(
                library.handle,
                device,
                &request,
                &mut session,
                &mut raw,
                &mut outcome,
            )
        };
        if category != CATEGORY_OK || session.is_null() {
            return Err(outcome_error(
                &outcome,
                "Initializing the hardware encoder session",
                false,
            ));
        }
        let required_format = match input_format {
            1 => 4,
            2 => 8,
            _ if main10 => 2,
            _ => 1,
        };
        if raw.codec != codec
            || raw.input_format != input_format
            || raw.caps_profile != 1
            || (main10 && raw.caps_10bit != 1)
            || raw.caps_input_formats & required_format == 0
        {
            close_prepared_session(session);
            return Err(internal_error(
                "The native encoder did not confirm the requested codec and 10-bit RGB input capability.".into(),
            ));
        }
        let settings = match settings_from_bridge(&raw) {
            Ok(settings) => settings,
            Err(error) => {
                close_prepared_session(session);
                return Err(error);
            }
        };
        // The driver must have applied exactly the requested configuration; a different
        // geometry, rate, bitrate, peak, GOP, rate-control mode or preset would make the
        // stream metadata a claim we cannot keep.
        if let Err(error) = settings.matches_request(config) {
            close_prepared_session(session);
            return Err(error);
        }
        if settings.width != config.width
            || settings.height != config.height
            || settings.rate_num != config.rate_num.get()
            || settings.rate_den != config.rate_den.get()
            || settings.bitrate_bps != config.bitrate_bps
            || settings.max_bitrate_bps != config.peak_bitrate_bps
            || settings.gop_length != config.gop_frames
            || settings.rate_control != config.rate_control
            || settings.preset != config.preset
        {
            close_prepared_session(session);
            return Err(internal_error(format!(
                "The native encoder reported {}x{} at {}/{} with {} bit/s (peak {}), GOP {}, {} and {}, not the requested {}x{} at {}/{} with {} bit/s (peak {}), GOP {}, {} and {}.",
                settings.width,
                settings.height,
                settings.rate_num,
                settings.rate_den,
                settings.bitrate_bps,
                settings.max_bitrate_bps,
                settings.gop_length,
                settings.rate_control.describe(),
                settings.preset.describe(),
                config.width,
                config.height,
                config.rate_num,
                config.rate_den,
                config.bitrate_bps,
                config.peak_bitrate_bps,
                config.gop_frames,
                config.rate_control.describe(),
                config.preset.describe()
            )));
        }
        Ok(Self {
            library,
            session,
            still_owned: false,
            settings,
            stats: NativeStats::default(),
            scratch: vec![0u8; SCRATCH_BYTES as usize],
            hdr_converter,
            colour,
        })
    }

    /// The exact NVENC call this session is inside right now, from the bridge itself.
    ///
    /// It is read without holding any lock across a native call, and it never contains
    /// pixel data.
    fn read_phase(&self) -> String {
        if self.session.is_null() {
            return "session-closed".to_string();
        }
        let mut phase = FrameNvencPhase::zeroed();
        // SAFETY: the session is alive and the out-parameter is a valid initialized value;
        // the bridge only reads its own diagnostic fields.
        unsafe { frame_nvenc_session_phase(self.session, &mut phase) };
        phase.describe()
    }
}

// The caller retains the device and library while this owner-thread cleanup runs.
fn close_prepared_session(session: *mut FrameNvencSession) {
    loop {
        let mut outcome = empty_outcome();
        // SAFETY: live prepared session; an unsafe result explicitly retains this pointer.
        let category = unsafe { frame_nvenc_session_close(session, &mut outcome) };
        if category != CATEGORY_CLEANUP_UNSAFE {
            return;
        }
        std::thread::sleep(super::UNCLEAN_RETRY_INTERVAL);
    }
}

impl FrameNvencSettings {
    pub(crate) fn zeroed() -> Self {
        // SAFETY: the bridge settings structure is plain data (integers and a byte
        // array) with no invalid bit patterns and no padding requirements.
        unsafe { std::mem::zeroed() }
    }
}

fn session_required(
    session: *mut FrameNvencSession,
) -> Result<*mut FrameNvencSession, RecorderError> {
    if session.is_null() {
        return Err(internal_error(
            "The native encoder session was used after it was closed.".to_string(),
        ));
    }
    Ok(session)
}

impl NvencDriver for NativeDriver {
    fn settings(&self) -> &NativeEncoderSettings {
        &self.settings
    }

    fn register_input(&mut self, texture: EncodeTexture) -> Result<u32, RecorderError> {
        let session = session_required(self.session)?;
        let hdr_input = self
            .hdr_converter
            .as_ref()
            .map(|converter| {
                converter.prepare_input(
                    texture.texture,
                    texture.identity.width,
                    texture.identity.height,
                )
            })
            .transpose()?;
        let native_texture = hdr_input
            .as_ref()
            .map_or(texture.texture, |input| input.target.as_raw());
        let mut registration = 0u32;
        let mut outcome = empty_outcome();
        // SAFETY: the session is alive, the texture pointer comes from the retained
        // capture payload, and the out-parameter is a valid initialized stack value.
        let category = unsafe {
            frame_nvenc_register_input(
                session,
                native_texture,
                texture.identity.width,
                texture.identity.height,
                &mut registration,
                &mut outcome,
            )
        };
        if category != CATEGORY_OK {
            return Err(outcome_error(
                &outcome,
                "Registering the capture texture with the hardware encoder",
                true,
            ));
        }
        if let (Some(converter), Some(input)) = (self.hdr_converter.as_mut(), hdr_input) {
            converter.retain(registration, input)?;
        }
        Ok(registration)
    }

    fn unregister_input(&mut self, registration: u32) -> Result<(), RecorderError> {
        let session = session_required(self.session)?;
        let mut outcome = empty_outcome();
        // SAFETY: the session is alive and the registration index came from
        // `register_input` on this session.
        let category = unsafe { frame_nvenc_unregister_input(session, registration, &mut outcome) };
        if category != CATEGORY_OK {
            return Err(outcome_error(
                &outcome,
                "Unregistering a capture texture from the hardware encoder",
                true,
            ));
        }
        if let Some(converter) = self.hdr_converter.as_mut() {
            converter.release(registration);
        }
        Ok(())
    }

    fn submit(
        &mut self,
        registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        force_idr: bool,
    ) -> Result<NativeSubmit, RecorderError> {
        let session = session_required(self.session)?;
        if let Some(converter) = self.hdr_converter.as_ref() {
            converter.convert(registration)?;
        }
        let mut slot = 0u32;
        let mut buffered = 0u32;
        let mut outcome = empty_outcome();
        // SAFETY: the session is alive, the registration belongs to it, and all
        // out-parameters are valid initialized stack values.
        let category = unsafe {
            frame_nvenc_submit(
                session,
                registration,
                pts_ns,
                duration_ns,
                u32::from(force_idr),
                &mut slot,
                &mut buffered,
                &mut outcome,
            )
        };
        if category == CATEGORY_OK {
            if buffered == 1 {
                // Buffered acceptance: an outstanding output slot the driver will fill
                // with later input or with EOS.
                return Ok(NativeSubmit::Buffered { slot });
            }
            return Ok(NativeSubmit::Pending { slot });
        }
        if is_busy(category) {
            // The bridge maps NEED_MORE_INPUT to a buffered OK above; anything left in the
            // busy category accepted nothing and must be retried unchanged.
            return Ok(NativeSubmit::Busy);
        }
        Err(outcome_error(
            &outcome,
            "Submitting a captured surface to the hardware encoder",
            true,
        ))
    }

    fn collect(
        &mut self,
        slot: u32,
        wait_ms: u32,
        capacity: usize,
    ) -> Result<NativeCollect, RecorderError> {
        let session = session_required(self.session)?;
        let bounded = u32::try_from(capacity).unwrap_or(MAX_ACCESS_UNIT_BYTES);
        if bounded == 0 || bounded > MAX_ACCESS_UNIT_BYTES {
            return Err(internal_error(format!(
                "The packet destination capacity {bounded} is outside the accepted 1..={MAX_ACCESS_UNIT_BYTES} access-unit bound."
            )));
        }
        let capacity = bounded as usize;
        let mut info = FrameNvencPacketInfo::default();
        let mut outcome = empty_outcome();
        // The scratch buffer is reused across polls: a pending poll copies nothing and
        // allocates nothing.
        // SAFETY: the session is alive, the scratch buffer is exclusively owned by this
        // driver and its length is passed as the checked capacity, and the out-parameters
        // are valid initialized stack values.
        let category = unsafe {
            frame_nvenc_collect(
                session,
                slot,
                wait_ms,
                self.scratch.as_mut_ptr(),
                bounded,
                &mut info,
                &mut outcome,
            )
        };
        if category != CATEGORY_OK {
            return Err(outcome_error(
                &outcome,
                "Collecting an encoded access unit from the hardware encoder",
                true,
            ));
        }
        if info.state == STATE_PENDING {
            return Ok(NativeCollect::Pending);
        }
        if info.state != STATE_READY {
            return Err(internal_error(format!(
                "The native encoder reported collection state {}, which is outside the bounded set.",
                info.state
            )));
        }
        let length = usize::try_from(info.bytes).unwrap_or(usize::MAX);
        if length == 0 || length > capacity || length > self.scratch.len() {
            return Err(internal_error(format!(
                "The native encoder reported {length} collected bytes, which does not fit the {capacity}-byte destination buffer."
            )));
        }
        // The delivered packet owns a copy of exactly the access-unit bytes; the scratch
        // buffer stays reusable for the next poll.
        let mut data = self.scratch[..length].to_vec();
        if matches!(self.settings.codec, 2 | 3)
            && info.keyframe == 1
            && let Some(colour) = self.colour
        {
            frame_encode::av1::preserve_hdr_metadata(&mut data, colour).map_err(internal_error)?;
        }
        let qp = self.stats.qp.get_or_insert(super::NativeQpStatistics {
            samples: 0,
            sum: 0,
            min: info.average_qp,
            max: info.average_qp,
        });
        qp.samples += 1;
        qp.sum += u64::from(info.average_qp);
        qp.min = qp.min.min(info.average_qp);
        qp.max = qp.max.max(info.average_qp);
        Ok(NativeCollect::Ready(NativePacket {
            data,
            pts_ns: info.pts_ns,
            duration_ns: info.duration_ns,
            keyframe: info.keyframe == 1,
            picture_type: info.picture_type,
            frame_index: info.frame_index,
        }))
    }

    fn begin_eos(&mut self) -> Result<NativeEos, RecorderError> {
        let session = session_required(self.session)?;
        let mut outcome = empty_outcome();
        // SAFETY: the session is alive and the outcome is a valid initialized value.
        let category = unsafe { frame_nvenc_begin_eos(session, &mut outcome) };
        if category == CATEGORY_OK {
            return Ok(NativeEos::Submitted);
        }
        if is_busy(category) {
            return Ok(NativeEos::Busy);
        }
        Err(outcome_error(
            &outcome,
            "Signalling end of stream to the hardware encoder",
            true,
        ))
    }

    fn poll_eos(&mut self, wait_ms: u32) -> Result<bool, RecorderError> {
        let session = session_required(self.session)?;
        let mut complete = 0u32;
        let mut outcome = empty_outcome();
        // SAFETY: the session is alive and both out-parameters are valid initialized
        // stack values.
        let category =
            unsafe { frame_nvenc_poll_eos(session, wait_ms, &mut complete, &mut outcome) };
        if category != CATEGORY_OK {
            return Err(outcome_error(
                &outcome,
                "Waiting for end-of-stream acknowledgement from the hardware encoder",
                true,
            ));
        }
        Ok(complete == 1)
    }

    fn phase(&self) -> String {
        self.read_phase()
    }

    fn stats(&self) -> NativeStats {
        if self.session.is_null() {
            return self.stats;
        }
        // The bridge counts the native calls and the documented busy/NEED_MORE_INPUT
        // results, so diagnostics report the driver's view rather than a Rust estimate.
        let mut calls = 0u32;
        let mut busy = 0u32;
        let mut pending = 0u32;
        // SAFETY: the session is alive and all three out-parameters are valid
        // initialized stack values; the bridge only reads its own counters.
        unsafe {
            frame_nvenc_session_stats(self.session, &mut calls, &mut busy, &mut pending);
        }
        NativeStats {
            qp: self.stats.qp,
            native_calls: calls,
            busy_results: busy,
            pending_inputs: pending,
        }
    }

    fn cleanup_unsafe(&self) -> bool {
        self.still_owned
    }

    fn close(&mut self) -> Result<(), RecorderError> {
        if self.session.is_null() {
            return Ok(());
        }
        // A close that cannot establish safe reuse is reported as its own condition, so the
        // worker can keep the corresponding leases and slots represented instead of
        // recycling textures the driver may still read.
        // Keep the driver's final counters available after the session is gone.
        let final_stats = self.stats();
        self.stats = final_stats;
        // The pointer is retained for the whole call: only a *confirmed* destruction may
        // clear it, so `still_owned` always agrees with a live handle that a retry and the
        // diagnostics can still reach.
        let session = self.session;
        let mut outcome = empty_outcome();
        // SAFETY: `session` is a live session created by this driver and is closed
        // by its owner thread. An unsafe result preserves the pointer for retry.
        let category = unsafe { frame_nvenc_session_close(session, &mut outcome) };
        if category == CATEGORY_CLEANUP_UNSAFE {
            // The bridge retained the whole owning graph, so this handle stays live: the
            // session pointer is deliberately NOT cleared, and the driver keeps owning it
            // until it is dropped by whoever retained this driver.
            self.still_owned = true;
            return Err(RecorderError {
                code: ErrorCode::Internal,
                message: format!(
                    "Native cleanup could not establish safe reuse: {}",
                    outcome_detail(&outcome)
                ),
                recovery: RecoveryStatus::Unknown,
                action: "The affected capture slots and registrations are kept alive instead of being recycled; report this run as a failed finalization."
                    .to_string(),
            });
        }
        self.still_owned = false;
        if category != CATEGORY_OK {
            // Confirmed destruction with a reported cleanup error: the handle is gone, so
            // the pointer is cleared and the session is no longer owned.
            self.session = ptr::null_mut();
            return Err(outcome_error(
                &outcome,
                "Releasing the hardware encoder session",
                true,
            ));
        }
        self.session = ptr::null_mut();
        Ok(())
    }
}

impl Drop for NativeDriver {
    fn drop(&mut self) {
        // Safety net only: the worker calls `close` explicitly so a cleanup failure can
        // be reported. `close` is idempotent, so this cannot double-release.
        loop {
            let _ = self.close();
            if !self.still_owned {
                break;
            }
            std::thread::sleep(super::UNCLEAN_RETRY_INTERVAL);
        }
        let _ = &self.library;
    }
}

/// One enumerated NVIDIA adapter that can host a qualification device.
struct AdapterCandidate {
    index: u32,
    description: String,
    luid: u64,
    vendor_id: u32,
    dedicated_video_memory: u64,
}

/// Enumerates DXGI adapters once, in adapter index order.
fn scan_adapters(factory: &IDXGIFactory1) -> (Vec<AdapterCandidate>, Option<RecorderError>) {
    let mut candidates = Vec::new();
    let mut index = 0u32;
    while (index as usize) < MAX_SCANNED_ENCODE_ADAPTERS {
        // SAFETY: adapter indices are probed until the documented not-found failure.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => return (candidates, None),
            Err(error) => {
                return (
                    candidates,
                    Some(internal_error(format!(
                        "IDXGIFactory1::EnumAdapters1({index}) failed with HRESULT 0x{:08X}, so the installed adapters cannot be qualified.",
                        error.code().0
                    ))),
                );
            }
        };
        // SAFETY: the adapter is alive for the duration of the call.
        let description: DXGI_ADAPTER_DESC1 = match unsafe { adapter.GetDesc1() } {
            Ok(description) => description,
            Err(error) => {
                return (
                    candidates,
                    Some(internal_error(format!(
                        "IDXGIAdapter1::GetDesc1({index}) failed with HRESULT 0x{:08X}, so this adapter cannot be qualified.",
                        error.code().0
                    ))),
                );
            }
        };
        candidates.push(AdapterCandidate {
            index,
            description: wide_to_string(&description.Description),
            luid: luid_value(description.AdapterLuid),
            vendor_id: description.VendorId,
            dedicated_video_memory: description.DedicatedVideoMemory as u64,
        });
        index += 1;
    }
    (
        candidates,
        Some(internal_error(format!(
            "More than {MAX_SCANNED_ENCODE_ADAPTERS} DXGI adapters were reported, which is outside the bounded qualification scan."
        ))),
    )
}

/// Read-only DXGI adapter list for adapter-scoped encoder diagnostics.
pub(crate) fn enumerate_adapter_identities()
-> Result<Vec<super::NativeAdapterIdentity>, RecorderError> {
    // SAFETY: CreateDXGIFactory1 returns an owned COM factory.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|error| {
        internal_error(format!(
            "CreateDXGIFactory1 failed with HRESULT 0x{:08X}.",
            error.code().0
        ))
    })?;
    let (adapters, failure) = scan_adapters(&factory);
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(adapters
        .into_iter()
        .map(|adapter| super::NativeAdapterIdentity {
            luid: adapter.luid,
            vendor_id: adapter.vendor_id,
            description: adapter.description,
        })
        .collect())
}

fn luid_value(luid: windows::Win32::Foundation::LUID) -> u64 {
    u64::from(luid.LowPart) | ((luid.HighPart as u32 as u64) << 32)
}

/// Driver package version for the exact LUID. Query IDXGIDevice, as documented
/// by DXGI; querying a D3D11 interface would incorrectly report unsupported.
pub(crate) fn adapter_driver_version(luid: u64) -> Result<u64, RecorderError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| internal_error(format!("Driver version factory: {e}")))?;
    for index in 0..MAX_SCANNED_ENCODE_ADAPTERS as u32 {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => {
                return Err(internal_error(format!(
                    "Driver version adapter scan: {error}"
                )));
            }
        };
        let desc = unsafe { adapter.GetDesc1() }
            .map_err(|e| internal_error(format!("Driver version adapter description: {e}")))?;
        if luid_value(desc.AdapterLuid) == luid {
            return unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
                .map(|version| version as u64)
                .map_err(|e| {
                    internal_error(format!(
                        "Driver version unavailable for adapter {luid:016X}: {e}"
                    ))
                });
        }
    }
    Err(internal_error(format!(
        "Driver version adapter {luid:016X} is no longer available."
    )))
}

/// The adapter attached to the exact D3D11 device owned by a capture lease.
pub(crate) fn device_adapter(
    device: *mut c_void,
) -> Result<super::NativeAdapterIdentity, RecorderError> {
    // SAFETY: the caller holds the capture payload owner for this entire call. The
    // borrowed COM interface does not take ownership of or release its reference.
    let device = unsafe { ID3D11Device::from_raw_borrowed(&device) }.ok_or_else(|| {
        internal_error("The capture lease has a null Direct3D 11 device.".to_string())
    })?;
    let dxgi: IDXGIDevice = device.cast().map_err(|error| {
        internal_error(format!(
            "The capture device has no IDXGIDevice (HRESULT 0x{:08X}).",
            error.code().0
        ))
    })?;
    // SAFETY: each returned COM interface owns its reference for the call.
    let adapter = unsafe { dxgi.GetAdapter() }.map_err(|error| {
        internal_error(format!(
            "IDXGIDevice::GetAdapter failed (HRESULT 0x{:08X}).",
            error.code().0
        ))
    })?;
    let adapter: IDXGIAdapter1 = adapter.cast().map_err(|error| {
        internal_error(format!(
            "The capture adapter has no IDXGIAdapter1 (HRESULT 0x{:08X}).",
            error.code().0
        ))
    })?;
    // SAFETY: the adapter is alive for the read-only description call.
    let desc = unsafe { adapter.GetDesc1() }.map_err(|error| {
        internal_error(format!(
            "IDXGIAdapter1::GetDesc1 failed (HRESULT 0x{:08X}).",
            error.code().0
        ))
    })?;
    Ok(super::NativeAdapterIdentity {
        luid: luid_value(desc.AdapterLuid),
        vendor_id: desc.VendorId,
        description: wide_to_string(&desc.Description),
    })
}

fn wide_to_string(slice: &[u16]) -> String {
    let length = slice
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..length])
}

/// Creates one temporary Direct3D 11 device on one adapter for qualification only.
pub(crate) fn qualification_device(
    factory: &IDXGIFactory1,
    adapter_index: u32,
) -> Result<ID3D11Device, RecorderError> {
    // SAFETY: the adapter index is one the bounded scan reported.
    let adapter = unsafe { factory.EnumAdapters1(adapter_index) }.map_err(|error| {
        internal_error(format!(
            "IDXGIFactory1::EnumAdapters1({adapter_index}) failed with HRESULT 0x{:08X} while creating the qualification device.",
            error.code().0
        ))
    })?;
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: the adapter is alive for the call, an explicit adapter requires
    // D3D_DRIVER_TYPE_UNKNOWN, and the out-parameter is a valid optional the API fills
    // on success. No software device is requested.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|error| {
        internal_error(format!(
            "D3D11CreateDevice failed on adapter {adapter_index} with HRESULT 0x{:08X}, so this adapter cannot be qualified.",
            error.code().0
        ))
    })?;
    device.ok_or_else(|| {
        internal_error(format!(
            "D3D11CreateDevice reported success on adapter {adapter_index} without a device."
        ))
    })
}

/// Temporary proof device on exactly the requested capture adapter.
pub(crate) fn qualification_device_for_luid(luid: u64) -> Result<ID3D11Device, RecorderError> {
    // SAFETY: the factory owns its COM reference for this bounded scan.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|error| {
        internal_error(format!(
            "CreateDXGIFactory1 failed: HRESULT 0x{:08X}.",
            error.code().0
        ))
    })?;
    let (adapters, failure) = scan_adapters(&factory);
    if let Some(error) = failure {
        return Err(error);
    }
    let adapter = adapters
        .iter()
        .find(|candidate| candidate.luid == luid)
        .ok_or_else(|| {
            internal_error(format!(
                "Capture adapter LUID {luid:016X} is not present in DXGI."
            ))
        })?;
    qualification_device(&factory, adapter.index)
}

/// One adapter's qualification attempt, kept as bounded diagnostic evidence.
pub(crate) struct QualificationAttempt {
    pub(crate) adapter: String,
    pub(crate) verified: bool,
    pub(crate) detail: String,
}

/// One verified candidate: its adapter name and the configuration its session reported.
pub(crate) type VerifiedCandidate = (super::NativeAdapterIdentity, NativeEncoderSettings);

/// Every adapter that was considered, in adapter order.
pub(crate) type QualificationAttempts = Vec<QualificationAttempt>;

/// Qualifies every NVIDIA adapter by initializing a real session for the requested
/// configuration and reading its initialization bytes back.
///
/// Vendor identification alone is never availability: a candidate exists only when the
/// driver opened a session, accepted the accepted ADR 0006 configuration, returned
/// initialization bytes and destroyed every resource again. Every considered adapter is
/// reported with the reason it was skipped or qualified.
pub(crate) fn probe_adapters(
    config: &NativeEncoderConfig,
) -> Result<(Vec<VerifiedCandidate>, QualificationAttempts), RecorderError> {
    probe_adapters_with_codec(config, 0)
}

pub(crate) fn probe_hevc_adapters(
    config: &NativeEncoderConfig,
) -> Result<(Vec<VerifiedCandidate>, QualificationAttempts), RecorderError> {
    probe_adapters_with_codec(config, 1)
}

pub(crate) fn probe_av1_adapters(
    config: &NativeEncoderConfig,
    main10: bool,
) -> Result<(Vec<VerifiedCandidate>, QualificationAttempts), RecorderError> {
    probe_adapters_with_codec(config, if main10 { 3 } else { 2 })
}

/// Exact adapter and configuration qualification used by recording selection.
pub(crate) fn probe_profile_for_adapter(
    adapter: &super::NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    profile: super::NativeVideoProfile,
) -> Result<NativeEncoderSettings, RecorderError> {
    config.validate()?;
    if !config.encoder_overrides.is_empty()
        && !matches!(
            profile,
            super::NativeVideoProfile::H264High | super::NativeVideoProfile::Av1Main8
        )
    {
        return Err(internal_error(
            "Native Advanced qualification is limited to SDR H.264/AV1.".into(),
        ));
    }
    let device = qualification_device_for_luid(adapter.luid)?;
    let library = NativeLibrary::acquire()?.0;
    let mut request = FrameNvencSessionRequest::from_config(config);
    request.codec = profile.bridge_value();
    let mut raw = FrameNvencSettings::zeroed();
    let mut outcome = empty_outcome();
    // SAFETY: library and device are retained through probe and confirmed cleanup.
    let result = unsafe {
        frame_nvenc_probe(
            library.handle,
            device.as_raw(),
            &request,
            &mut raw,
            &mut outcome,
        )
    };
    if result != CATEGORY_OK {
        return Err(outcome_error(
            &outcome,
            "Qualifying the selected native codec/profile",
            false,
        ));
    }
    if raw.codec != request.codec || raw.caps_profile != 1 {
        return Err(internal_error(
            "Native qualification did not confirm the requested profile.".into(),
        ));
    }
    let settings = settings_from_bridge(&raw)?;
    settings.matches_request(config)?;
    Ok(settings)
}

fn probe_adapters_with_codec(
    config: &NativeEncoderConfig,
    codec: u32,
) -> Result<(Vec<VerifiedCandidate>, QualificationAttempts), RecorderError> {
    let library = NativeLibrary::acquire()?.0;
    // SAFETY: CreateDXGIFactory1 has no preconditions and returns an owned COM object.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|error| {
        internal_error(format!(
            "CreateDXGIFactory1 failed with HRESULT 0x{:08X}, so encoder qualification cannot enumerate adapters.",
            error.code().0
        ))
    })?;
    let (adapters, failure) = scan_adapters(&factory);
    let mut verified = Vec::new();
    let mut attempts = Vec::new();
    for adapter in &adapters {
        if adapter.vendor_id != NVIDIA_VENDOR_ID {
            attempts.push(QualificationAttempt {
                adapter: adapter.description.clone(),
                verified: false,
                detail: format!(
                    "Skipped: vendor 0x{:04X} is not NVIDIA (0x{NVIDIA_VENDOR_ID:04X}); this path has no other-provider fallback.",
                    adapter.vendor_id
                ),
            });
            continue;
        }
        let device = match qualification_device(&factory, adapter.index) {
            Ok(device) => device,
            Err(error) => {
                attempts.push(QualificationAttempt {
                    adapter: adapter.description.clone(),
                    verified: false,
                    detail: error.message,
                });
                continue;
            }
        };
        let device_pointer = device.as_raw();
        let mut raw = FrameNvencSettings::zeroed();
        let mut outcome = empty_outcome();
        let main10 = codec == 1 || codec == 3;
        let mut request = FrameNvencSessionRequest::from_config(config);
        request.codec = codec;
        // SAFETY: the device is alive for the call, the request is a plain POD value and
        // both out-parameters are valid initialized stack values. The device is dropped
        // immediately afterwards.
        let category = unsafe {
            frame_nvenc_probe(
                library.handle,
                device_pointer,
                &request,
                &mut raw,
                &mut outcome,
            )
        };
        drop(device);
        if category != CATEGORY_OK {
            attempts.push(QualificationAttempt {
                adapter: adapter.description.clone(),
                verified: false,
                detail: outcome_detail(&outcome),
            });
            continue;
        }
        if raw.codec != codec
            || (main10 && (raw.caps_10bit != 1 || raw.caps_input_abgr10 != 1))
            || (!main10 && raw.caps_input_argb != 1)
        {
            attempts.push(QualificationAttempt {
                adapter: adapter.description.clone(),
                verified: false,
                detail: "The native session did not confirm the requested codec and input depth capability."
                    .into(),
            });
            continue;
        }
        match settings_from_bridge(&raw) {
            Ok(settings) => {
                attempts.push(QualificationAttempt {
                    adapter: adapter.description.clone(),
                    verified: true,
                    detail: format!(
                        "Qualified {}x{} at {}/{} with {} bit/s (peak {}) {}, {}, GOP {}, {} initialization bytes, {} MiB dedicated video memory.",
                        settings.width,
                        settings.height,
                        settings.rate_num,
                        settings.rate_den,
                        settings.bitrate_bps,
                        settings.max_bitrate_bps,
                        settings.rate_control.describe(),
                        settings.preset.describe(),
                        settings.gop_length,
                        settings.codec_config.len(),
                        adapter.dedicated_video_memory / (1024 * 1024)
                    ),
                });
                verified.push((
                    super::NativeAdapterIdentity {
                        luid: adapter.luid,
                        vendor_id: adapter.vendor_id,
                        description: adapter.description.clone(),
                    },
                    settings,
                ));
            }
            Err(error) => attempts.push(QualificationAttempt {
                adapter: adapter.description.clone(),
                verified: false,
                detail: error.message,
            }),
        }
    }
    if let Some(error) = failure {
        // A scan failure is reported instead of being folded into "no candidates".
        return Err(error);
    }
    Ok((verified, attempts))
}

/// Library information for diagnostics, or the explicit load failure.
pub(crate) fn library_report() -> (Option<NativeLibraryInfo>, String) {
    match NativeLibrary::acquire() {
        Ok((library, info)) => {
            let detail =
                "loaded from the Windows system directory and released cleanly during this report"
                    .to_string();
            drop(library);
            (Some(info), detail)
        }
        Err(error) => (None, error.message),
    }
}
