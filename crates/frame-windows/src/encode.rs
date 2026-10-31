//! Windows hardware H.264 encoding (FRAME-0104, ADR 0006).
//!
//! This module owns the bounded, realtime policy of the first encoder path: admission,
//! capture-lease retention, packet-byte credits, native completion accounting, explicit
//! end of stream and exactly-once teardown. Everything native lives behind the private
//! [`driver::NvencDriver`] seam, whose production implementation is the C bridge in
//! [`native`] and whose test implementation is scripted, so pressure, credits, drain
//! states, failures and unwinds are covered without a GPU.
//!
//! What this module deliberately does **not** do: record, mux, container, audio,
//! timeline, UI or IPC work. It compiles a bounded Annex-B H.264 elementary stream from
//! captured surfaces. `Pending` is never drain completion, an accepted input is never
//! silently dropped, and a stalled consumer blocks admission instead of growing memory.
//!
//! Bounds (ADR 0006, all enforced here):
//!
//! - at most four capture leases exist across the proof caller's lookahead frame, this
//!   encoder's ingress and its native in-flight work, so the encoder itself retains at
//!   most [`MAX_ENCODER_LEASES`] of them;
//! - at most four owned packets, 40 MiB including the 8 MiB native collector scratch,
//!   with 8 MiB per access
//!   unit and a full 8 MiB credit reserved before any native submission;
//! - at most 64 KiB of codec initialization bytes;
//! - four native output buffers/events plus a separate EOS event;
//! - native waits of at most 5 ms with 5 s preparation, progress and drain watchdogs.

mod artifacts;
mod driver;
mod fallback;
mod mf_transfer;
pub use mf_transfer::{MfStageTiming, MfTransferFormat, MfTransferStats};
#[allow(unsafe_code)]
mod hdr_convert;
#[allow(unsafe_code)]
mod media_foundation;
#[allow(unsafe_code)]
mod mf_process;
#[allow(unsafe_code)]
mod nv12_convert;
#[allow(unsafe_code)]
mod shared_texture_proof;

pub(crate) use shared_texture_proof::{
    shared_texture_child_entry, shared_texture_parent_proof, shared_texture_watchdog_proof,
};

pub(crate) fn mf_helper_entry() -> i32 {
    mf_process::run_helper()
}
#[allow(unsafe_code)]
mod native;
#[cfg(test)]
#[allow(unsafe_code)]
mod tests;

pub use artifacts::ProofArtifacts;

use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use frame_capture::CapturedFrame;
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, EncoderInput,
    EncoderPoll, PacketEncoder, SubmitOutcome, VideoColour,
};

use crate::surface::{SharedPayload, SurfaceFormat, WindowsSurface};

use driver::{
    EncodeTexture, NativeCollect, NativeEos, NativeStats, NativeSubmit, NvencDriver,
    SurfaceIdentity,
};

/// The accepted proof bitrate (ADR 0006: a documented proof setting, not product
/// quality policy).
pub const PROOF_BITRATE_BPS: u32 = 40_000_000;
/// The accepted proof GOP / IDR interval in accepted frames.
pub const PROOF_GOP_FRAMES: u32 = 120;
/// At most four owned packets (ADR 0006).
pub const MAX_PACKETS: usize = 4;
/// Four maximum-size access units plus the native collector's scratch buffer.
/// Charging scratch inside the old 32 MiB cap prevented the fourth output credit.
pub const MAX_PACKET_BYTES: u64 = 40 * 1024 * 1024;
/// At most 8 MiB per access unit (ADR 0006).
pub const MAX_ACCESS_UNIT_BYTES: u32 = 8 * 1024 * 1024;
/// At most 64 KiB of codec initialization bytes (ADR 0006).
pub const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;
/// The qualification geometry and rate ADR 0006 requires for discovery.
pub const QUALIFICATION_WIDTH: u32 = 1920;
pub const QUALIFICATION_HEIGHT: u32 = 1080;
pub const QUALIFICATION_RATE_NUM: u32 = 60;
pub const QUALIFICATION_RATE_DEN: u32 = 1;
/// Bound on the qualification adapter scan.
pub(crate) const MAX_SCANNED_ENCODE_ADAPTERS: usize = 16;
/// One of the four capture slots is the proof caller's lookahead frame, so this encoder
/// retains at most three leases across its ingress queue and native in-flight work.
pub(crate) const MAX_ENCODER_LEASES: usize = 3;
/// Native texture registrations are bounded to one capture pool's four textures.
const MAX_REGISTRATIONS: usize = 4;
/// Bounded ingress capacity, in jobs, for the same lease reason.
const INGRESS_CAPACITY: usize = MAX_ENCODER_LEASES;
/// Native event waits never exceed this (ADR 0006: at most 5 ms between checks).
const NATIVE_WAIT_MS: u32 = 1;
/// Bounded interval between retries of a native cleanup that could not confirm completion.
const UNCLEAN_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Preparation, native-progress and drain watchdogs (ADR 0006: five seconds).
const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);
const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);
const DRAIN_WATCHDOG: Duration = Duration::from_secs(5);
/// Idle wait, so a stop or drain request is observed promptly without spinning.
const IDLE_POLL: Duration = Duration::from_millis(2);

// --------------------------------------------------------------------- errors

pub(crate) fn internal_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::Unknown,
        action: "Discard the incomplete stream and retry the encode.".to_string(),
    }
}

pub(crate) fn preparation_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "No stream was produced; review the reported condition and retry.".to_string(),
    }
}

pub(crate) fn invalid_request_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Review the encoder request and try again.".to_string(),
    }
}

pub(crate) fn finalization_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::FinalizationFailed,
        message,
        recovery: RecoveryStatus::Unknown,
        action: "The stream is incomplete: discard the artifact and retry the encode.".to_string(),
    }
}

// -------------------------------------------------------------- configuration

/// The rate-control mode a session is initialized with (FRAME-0207).
///
/// The caller's policy chooses it; the bridge applies exactly the requested mode and
/// fails instead of substituting another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NativeRateControl {
    Cbr,
    Vbr,
    Cqp,
    Lossless,
}

impl NativeRateControl {
    /// The Frame-owned ABI value declared in `native/nvenc_bridge.h`.
    pub(crate) fn bridge_value(self) -> u32 {
        match self {
            Self::Cbr => 0,
            Self::Vbr => 1,
            Self::Cqp => 3,
            Self::Lossless => 2,
        }
    }

    fn from_bridge(value: u32) -> Result<Self, RecorderError> {
        match value {
            0 => Ok(Self::Cbr),
            1 => Ok(Self::Vbr),
            3 => Ok(Self::Cqp),
            2 => Ok(Self::Lossless),
            other => Err(internal_error(format!(
                "The native encoder reported rate-control mode {other}, which is outside the bounded set this crate declares."
            ))),
        }
    }

    /// The mode as it is reported in evidence.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Cbr => "CBR",
            Self::Vbr => "VBR",
            Self::Cqp => "CQP",
            Self::Lossless => "Lossless QP 0",
        }
    }
}

/// The preset/tuning pair a session is initialized with (FRAME-0207).
///
/// It changes how the encoder spends the planned bitrate; it does not change the frame
/// model. Every pair is requested from the driver, verified before initialization and
/// reported back, so an unavailable preset is an explicit failure and never a silent
/// substitution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NativeEncoderPreset {
    Lossless,
    /// P1 with ultra-low-latency tuning: the Standard tier and the existing proof path.
    LowLatency,
    /// P5 with high-quality tuning: the High tier.
    HighQuality,
    /// P7 with high-quality tuning: the Maximum tier.
    MaximumQuality,
}

impl NativeEncoderPreset {
    /// The Frame-owned ABI value declared in `native/nvenc_bridge.h`.
    pub(crate) fn bridge_value(self) -> u32 {
        match self {
            Self::Lossless => 3,
            Self::LowLatency => 0,
            Self::HighQuality => 1,
            Self::MaximumQuality => 2,
        }
    }

    fn from_bridge(value: u32) -> Result<Self, RecorderError> {
        match value {
            3 => Ok(Self::Lossless),
            0 => Ok(Self::LowLatency),
            1 => Ok(Self::HighQuality),
            2 => Ok(Self::MaximumQuality),
            other => Err(internal_error(format!(
                "The native encoder reported preset {other}, which is outside the bounded set this crate declares."
            ))),
        }
    }

    /// The preset as it is reported in evidence.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Lossless => "P1 / lossless YUV 4:4:4",
            Self::LowLatency => "P1 / ultra-low-latency",
            Self::HighQuality => "P5 / high-quality",
            Self::MaximumQuality => "P7 / high-quality",
        }
    }
}

/// The quality half of a configuration: what the rate policy decided (FRAME-0207).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQualitySettings {
    /// The average bitrate the mode targets.
    pub bitrate_bps: u32,
    /// The peak bitrate the mode may use; equal to `bitrate_bps` for CBR.
    pub peak_bitrate_bps: u32,
    /// The keyframe and IDR interval, in accepted frames.
    pub gop_frames: u32,
    pub rate_control: NativeRateControl,
    pub preset: NativeEncoderPreset,
}

/// The explicitly reported proof configuration (FRAME-0104).
///
/// The nominal rate is an explicit positive rational: it is the configuration the
/// encoder is initialized with and is never derived from a captured `f64`, rounded, or
/// presented as a measured throughput.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeEncoderConfig {
    #[serde(default)]
    pub encoder_overrides: frame_contracts::VideoEncoderOverrides,
    pub width: u32,
    pub height: u32,
    pub rate_num: NonZeroU32,
    pub rate_den: NonZeroU32,
    pub bitrate_bps: u32,
    /// The peak bitrate the applied mode may use; equal to `bitrate_bps` for CBR.
    pub peak_bitrate_bps: u32,
    pub gop_frames: u32,
    /// The rate-control mode the session is initialized with.
    pub rate_control: NativeRateControl,
    /// The preset/tuning pair the session is initialized with.
    pub preset: NativeEncoderPreset,
}

impl NativeEncoderConfig {
    /// The accepted ADR 0006 proof configuration for one geometry and rational rate:
    /// CBR [`PROOF_BITRATE_BPS`], a [`PROOF_GOP_FRAMES`] GOP, auto level, forced initial
    /// IDR and repeated SPS/PPS at IDRs. The native session verifies the geometry, the
    /// rate, the format and the asynchronous-completion capability before it initializes.
    pub fn proof(
        width: u32,
        height: u32,
        rate_num: NonZeroU32,
        rate_den: NonZeroU32,
    ) -> Result<Self, RecorderError> {
        let config = Self {
            encoder_overrides: Default::default(),
            width,
            height,
            rate_num,
            rate_den,
            bitrate_bps: PROOF_BITRATE_BPS,
            peak_bitrate_bps: PROOF_BITRATE_BPS,
            gop_frames: PROOF_GOP_FRAMES,
            rate_control: NativeRateControl::Cbr,
            preset: NativeEncoderPreset::LowLatency,
        };
        config.validate()?;
        Ok(config)
    }

    /// The same bounded configuration with a rate policy's quality settings applied.
    ///
    /// This is the one place a planned bitrate, peak, GOP, rate-control mode and preset
    /// enter the encoder: the resulting configuration is re-validated, and the native
    /// session then has to report exactly these values back before a frame is accepted.
    pub fn with_quality(
        width: u32,
        height: u32,
        rate_num: NonZeroU32,
        rate_den: NonZeroU32,
        quality: NativeQualitySettings,
    ) -> Result<Self, RecorderError> {
        let config = Self {
            encoder_overrides: Default::default(),
            width,
            height,
            rate_num,
            rate_den,
            bitrate_bps: quality.bitrate_bps,
            peak_bitrate_bps: quality.peak_bitrate_bps,
            gop_frames: quality.gop_frames,
            rate_control: quality.rate_control,
            preset: quality.preset,
        };
        config.validate()?;
        Ok(config)
    }

    /// The explicit qualification configuration discovery uses.
    pub fn qualification() -> Result<Self, RecorderError> {
        let num = NonZeroU32::new(QUALIFICATION_RATE_NUM).ok_or_else(|| {
            invalid_request_error("The qualification rate numerator must be positive.".to_string())
        })?;
        let den = NonZeroU32::new(QUALIFICATION_RATE_DEN).ok_or_else(|| {
            invalid_request_error(
                "The qualification rate denominator must be positive.".to_string(),
            )
        })?;
        Self::proof(QUALIFICATION_WIDTH, QUALIFICATION_HEIGHT, num, den)
    }

    /// Rejects a request that cannot become an 8-bit 4:2:0 H.264 stream.
    pub fn validate(&self) -> Result<(), RecorderError> {
        if self.width == 0 || self.height == 0 {
            return Err(invalid_request_error(
                "The encoder needs a positive width and height.".to_string(),
            ));
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(invalid_request_error(format!(
                "A {}x{} encoder request cannot be encoded as 8-bit 4:2:0, which requires even dimensions.",
                self.width, self.height
            )));
        }
        self.encoder_overrides.validate()?;
        let a = self.encoder_overrides;
        if !a.is_empty() && self.preset == NativeEncoderPreset::Lossless {
            return Err(invalid_request_error(
                "Lossless and native Advanced overrides cannot be combined.".into(),
            ));
        }
        if a.rate_control.is_some_and(|rc| match rc {
            frame_contracts::VideoRateControl::Cbr => self.rate_control != NativeRateControl::Cbr,
            frame_contracts::VideoRateControl::Vbr => self.rate_control != NativeRateControl::Vbr,
            frame_contracts::VideoRateControl::Cqp => self.rate_control != NativeRateControl::Cqp,
            _ => true,
        }) || a.gop_frames.is_some_and(|g| g != self.gop_frames)
        {
            return Err(invalid_request_error(
                "Native configuration differs from explicit Advanced intent.".into(),
            ));
        }
        if self.rate_control == NativeRateControl::Cqp
            && (self.bitrate_bps != 0
                || self.peak_bitrate_bps != 0
                || self.encoder_overrides.qp.is_none())
        {
            return Err(invalid_request_error(
                "CQP requires QP and zero bitrate fields.".into(),
            ));
        }
        if self.bitrate_bps == 0
            && self.preset != NativeEncoderPreset::Lossless
            && self.rate_control != NativeRateControl::Cqp
        {
            return Err(invalid_request_error(
                "The encoder needs a positive bitrate.".to_string(),
            ));
        }
        if self.peak_bitrate_bps < self.bitrate_bps {
            return Err(invalid_request_error(format!(
                "The peak bitrate {} bit/s is below the average bitrate {} bit/s.",
                self.peak_bitrate_bps, self.bitrate_bps
            )));
        }
        if self.gop_frames == 0 {
            return Err(invalid_request_error(
                "The encoder needs a positive GOP length.".to_string(),
            ));
        }
        // The nominal rational rate is explicit and positive by construction; whether the
        // hardware and the driver accept it is decided by capability queries and
        // initialization, not by an invented ceiling here.
        Ok(())
    }
}

/// Snapshot of fields passed to successful NvEncInitializeEncoder, not a driver query.
/// Sequence headers and packet statistics provide independent output evidence.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeAdvancedSettings {
    pub preset_index: u32,
    pub tuning: u32,
    pub qp: u32,
    pub multipass: u32,
    pub spatial_aq: u32,
    pub temporal_aq: u32,
    pub caps_max_b_frames: u32,
    pub caps_lookahead: u32,
    pub caps_temporal_aq: u32,
}

/// Accepted initialization snapshot plus actual driver capabilities and sequence bytes.
///
/// This is evidence, not a restatement of the request: the capability answers and the
/// output-buffer bound come from the driver, and the initialization bytes are the
/// codec-defined SPS/PPS it returned.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeEncoderSettings {
    #[serde(default)]
    pub advanced: NativeAdvancedSettings,
    /// Frame bridge codec: H.264=0, HEVC Main10=1, AV1 8-bit=2, AV1 10-bit=3.
    #[serde(default)]
    pub codec: u32,
    /// Packed RGB=0, NV12=1, P010=2. This is the actual native input.
    #[serde(default)]
    pub input_format: u32,
    /// Driver input-format flags: BGRA=1, RGB10=2, NV12=4, P010=8.
    #[serde(default)]
    pub caps_input_formats: u32,
    #[serde(default)]
    pub caps_profile: bool,
    #[serde(default)]
    pub caps_10bit: bool,
    pub width: u32,
    pub height: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    pub bitrate_bps: u32,
    /// The peak bitrate the applied mode may use; equal to `bitrate_bps` for CBR.
    pub max_bitrate_bps: u32,
    pub gop_length: u32,
    pub idr_period: u32,
    pub frame_interval_p: u32,
    pub enable_ptd: bool,
    pub enable_async: bool,
    pub repeat_spspps: bool,
    pub level: u32,
    pub profile_high: bool,
    pub vui_full_range: bool,
    /// The rate-control mode passed to successful native initialization.
    pub rate_control: NativeRateControl,
    /// The inherited policy pair; `advanced` carries the exact native preset/tuning.
    pub preset: NativeEncoderPreset,
    pub caps_async_encode: bool,
    pub caps_width_max: u32,
    pub caps_height_max: u32,
    pub caps_rate_control_modes: u32,
    pub caps_h264_guid: bool,
    pub caps_input_argb: bool,
    pub caps_profile_high: bool,
    /// Whether the requested preset/tuning pair was obtained from the driver.
    pub caps_preset_available: bool,
    pub output_buffers: u32,
    pub output_buffer_capacity: u32,
    pub max_access_unit_bytes: u32,
    /// Codec-defined initialization bytes (Annex-B SPS/PPS).
    pub codec_config: Vec<u8>,
}

/// Real per-frame average QP returned by NV_ENC_LOCK_BITSTREAM::frameAvgQP.
/// Live snapshots may be incomplete; after drain the sample count is final.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeQpStatistics {
    pub samples: u64,
    pub sum: u64,
    pub min: u32,
    pub max: u32,
}

/// The rate-control modes one qualified encoder reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeRateControlSupport {
    pub cbr: bool,
    pub vbr: bool,
}

/// Profiles implemented by the native bridge, each qualified by the actual driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NativeVideoProfile {
    H264High,
    HevcMain10,
    Av1Main8,
    Av1Main10,
}

impl NativeVideoProfile {
    pub(crate) fn bridge_value(self) -> u32 {
        match self {
            Self::H264High => 0,
            Self::HevcMain10 => 1,
            Self::Av1Main8 => 2,
            Self::Av1Main10 => 3,
        }
    }
    pub fn codec(self) -> &'static str {
        match self {
            Self::H264High => "h264",
            Self::HevcMain10 => "hevc",
            _ => "av1",
        }
    }
    pub fn bit_depth(self) -> u8 {
        if matches!(self, Self::HevcMain10 | Self::Av1Main10) {
            10
        } else {
            8
        }
    }
    pub fn id(self, luid: u64) -> String {
        let name = match self {
            Self::H264High => "h264",
            Self::HevcMain10 => "hevc-main10",
            Self::Av1Main8 => "av1",
            Self::Av1Main10 => "av1-main10",
        };
        format!("nvidia-nvenc-{name}-d3d11:luid={luid:016X}")
    }
    fn bitstream(self) -> BitstreamFormat {
        match self {
            Self::H264High => BitstreamFormat::H264AnnexB,
            Self::HevcMain10 => BitstreamFormat::HevcAnnexB,
            _ => BitstreamFormat::Av1Obu,
        }
    }
}

fn native_input_signal(
    profile: NativeVideoProfile,
    format: SurfaceFormat,
    signal: Option<crate::surface::SurfaceSignal>,
) -> Result<(u32, VideoColour, bool), RecorderError> {
    if format == SurfaceFormat::ScRgbFloat16 {
        let source =
            signal.ok_or_else(|| preparation_error("scRGB requires its source signal.".into()))?;
        let mut base = source;
        base.colour.mastering = None;
        base.colour.content_light = None;
        if profile.bit_depth() != 10 || base != crate::surface::SurfaceSignal::SCRGB {
            return Err(preparation_error(
                "scRGB requires its linear source signal and a 10-bit profile.".into(),
            ));
        }
        source.colour.validate()?;
        let mut output = VideoColour::BT2020_PQ_LIMITED;
        output.mastering = source.colour.mastering;
        output.content_light = source.colour.content_light;
        return Ok((0, output, true));
    }
    if format == SurfaceFormat::Bgra8 {
        if profile.bit_depth() != 8 || signal.is_some() {
            return Err(preparation_error(
                "Packed BGRA accepts the established BT.709 SDR conversion only.".into(),
            ));
        }
        return Ok((0, VideoColour::BT709_LIMITED, false));
    }
    let colour = signal
        .ok_or_else(|| {
            preparation_error("Native YUV requires source colour/range metadata.".into())
        })?
        .colour;
    colour.validate()?;
    let (input, depth) = match format {
        SurfaceFormat::Nv12 => (1, 8),
        SurfaceFormat::P010 => (2, 10),
        _ => unreachable!(),
    };
    if depth != profile.bit_depth()
        || colour.bits_per_channel != depth
        || colour.matrix_coefficients == 0
        || colour.transfer_characteristics == 8
        || (profile == NativeVideoProfile::H264High
            && (colour.mastering.is_some() || colour.content_light.is_some()))
    {
        return Err(preparation_error(
            "Native YUV bit depth or signal is unsupported by the requested profile.".into(),
        ));
    }
    Ok((input, colour, false))
}

impl NativeEncoderSettings {
    pub fn preset_description(&self) -> String {
        if self.advanced.preset_index != 0 {
            let tuning = match self.advanced.tuning {
                1 => "high quality",
                2 => "low latency",
                3 => "ultra-low latency",
                4 => "lossless",
                _ => "unknown tuning",
            };
            format!("P{} / {tuning}", self.advanced.preset_index)
        } else {
            self.preset.describe().into()
        }
    }
    /// Refuse a driver that accepted initialization but applied a different plan.
    fn matches_request(&self, requested: &NativeEncoderConfig) -> Result<(), RecorderError> {
        if self.width != requested.width
            || self.height != requested.height
            || self.rate_num != requested.rate_num.get()
            || self.rate_den != requested.rate_den.get()
            || self.bitrate_bps != requested.bitrate_bps
            || self.max_bitrate_bps != requested.peak_bitrate_bps
            || self.gop_length != requested.gop_frames
            || self.idr_period != requested.gop_frames
            || self.rate_control != requested.rate_control
            || self.preset != requested.preset
        {
            return Err(preparation_error(format!(
                "The hardware encoder applied {} but the requested geometry, rate, bitrate, GOP or quality differs.",
                self.describe()
            )));
        }
        let a = requested.encoder_overrides;
        // Legacy paths use zeroed evidence. Advanced sessions must carry exact accepted fields.
        if !a.is_empty()
            && (a
                .preset
                .is_some_and(|v| self.advanced.preset_index != v as u32 + 1)
                || a.tuning.is_some_and(|v| {
                    self.advanced.tuning
                        != match v {
                            frame_contracts::VideoEncoderTuning::HighQuality => 1,
                            frame_contracts::VideoEncoderTuning::LowLatency => 2,
                            frame_contracts::VideoEncoderTuning::UltraLowLatency => 3,
                        }
                })
                || a.qp.is_some_and(|v| self.advanced.qp != v)
                || a.multipass
                    .is_some_and(|v| self.advanced.multipass != v as u32)
                || a.spatial_aq
                    .is_some_and(|v| self.advanced.spatial_aq != u32::from(v))
                || a.temporal_aq
                    .is_some_and(|v| self.advanced.temporal_aq != u32::from(v)))
        {
            return Err(preparation_error(
                "Native Advanced initialization snapshot differs from requested intent.".into(),
            ));
        }
        Ok(())
    }

    /// The rate-control modes this candidate reported, translated out of the NVIDIA
    /// capability bitmask (`NV_ENC_PARAMS_RC_VBR = 0x1`, `NV_ENC_PARAMS_RC_CBR = 0x2`)
    /// so a rate policy outside this crate never reads an SDK bitmask.
    pub fn rate_control_support(&self) -> NativeRateControlSupport {
        NativeRateControlSupport {
            cbr: self.caps_rate_control_modes & 0x2 != 0,
            vbr: self.caps_rate_control_modes & 0x1 != 0,
        }
    }

    /// One bounded line of the effective configuration.
    fn describe(&self) -> String {
        format!(
            "{}x{} at {}/{} nominal, {} {} bit/s (peak {}), GOP {} (IDR every {}), frameIntervalP={}, ptd={}, async={}, repeatSPSPPS={}, level={}, profile_high={}, vui_full_range={}, preset={}, {} initialization bytes, {} output buffers of {} bytes ({} byte access-unit bound)",
            self.width,
            self.height,
            self.rate_num,
            self.rate_den,
            self.rate_control.describe(),
            self.bitrate_bps,
            self.max_bitrate_bps,
            self.gop_length,
            self.idr_period,
            self.frame_interval_p,
            self.enable_ptd,
            self.enable_async,
            self.repeat_spspps,
            self.level,
            self.profile_high,
            self.vui_full_range,
            self.preset_description(),
            self.codec_config.len(),
            self.output_buffers,
            self.output_buffer_capacity,
            self.max_access_unit_bytes
        )
    }
}

/// One verified hardware encoder candidate from a real qualification session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeAdapterIdentity {
    /// Locally unique DXGI adapter identity, not a display name or a portable ID.
    pub luid: u64,
    pub vendor_id: u32,
    pub description: String,
}

pub use media_foundation::MfH264Preparation;
pub use mf_process::prepare_hardware_h264;

/// Read-only adapter identities from one bounded DXGI scan.
pub fn enumerate_native_adapters() -> Result<Vec<NativeAdapterIdentity>, RecorderError> {
    native::enumerate_adapter_identities()
}

/// Exact DXGI user-mode driver package version, or an explicit query failure.
pub fn adapter_driver_version(luid: u64) -> Result<u64, RecorderError> {
    native::adapter_driver_version(luid)
}

#[derive(Clone, Debug)]
pub struct NativeEncoderCandidate {
    pub capability: EncoderCapability,
    /// The adapter the qualification session was created on.
    pub adapter: String,
    pub adapter_luid: u64,
    /// The configuration that session actually initialized with.
    pub settings: NativeEncoderSettings,
}

/// Qualifies a supported codec/profile for the exact capture adapter and plan.
/// Presence in an SDK or a vendor name is insufficient for admission.
pub fn probe_native_profile_for_adapter(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    profile: NativeVideoProfile,
) -> Result<NativeEncoderCandidate, RecorderError> {
    config.validate()?;
    if adapter.vendor_id != 0x10DE {
        return Err(preparation_error(
            "The selected native codec/profile has no implemented backend on this adapter.".into(),
        ));
    }
    let settings = native::probe_profile_for_adapter(adapter, config, profile)?;
    Ok(NativeEncoderCandidate {
        capability: EncoderCapability {
            id: profile.id(adapter.luid),
            codec: profile.codec().into(),
            hardware: true,
        },
        adapter: adapter.description.clone(),
        adapter_luid: adapter.luid,
        settings,
    })
}

/// Implemented native profiles on each adapter. H.264 remains first and is
/// kept if optional profiles fail qualification. HDR capture/export availability
/// must be checked independently of these encoder-only capabilities.
pub fn probe_native_profiles() -> Result<Vec<NativeEncoderCandidate>, RecorderError> {
    let mut candidates = probe_native_encoders().unwrap_or_default();
    let config = NativeEncoderConfig::qualification()?;
    for adapter in enumerate_native_adapters()? {
        if adapter.vendor_id == 0x10DE {
            for profile in [
                NativeVideoProfile::HevcMain10,
                NativeVideoProfile::Av1Main8,
                NativeVideoProfile::Av1Main10,
            ] {
                if let Ok(candidate) = probe_native_profile_for_adapter(&adapter, &config, profile)
                {
                    candidates.push(candidate);
                }
            }
        } else if matches!(adapter.vendor_id, 0x8086 | 0x1002)
            && let Ok(mut qualified) = probe_h264_encoder_for_adapter(
                &adapter,
                (config.width, config.height),
                (config.rate_num, config.rate_den),
            )
        {
            candidates.append(&mut qualified);
        }
        if matches!(adapter.vendor_id, 0x10DE | 0x8086 | 0x1002)
            && let Ok(candidate) = probe_mf_av1_for_adapter(&adapter, &config)
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// AV1 Main 8-bit through adapter-scoped hardware Media Foundation. Qualification
/// includes GPU submission, bitstream/profile/colour validation and completed EOS.
pub fn probe_mf_av1_for_adapter(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<NativeEncoderCandidate, RecorderError> {
    let settings = mf_process::qualify_profile(adapter, config, 2)?;
    Ok(NativeEncoderCandidate {
        capability: EncoderCapability {
            id: mf_av1_id(adapter)?,
            codec: "av1".into(),
            hardware: true,
        },
        adapter: adapter.description.clone(),
        adapter_luid: adapter.luid,
        settings,
    })
}

fn mf_av1_id(adapter: &NativeAdapterIdentity) -> Result<String, RecorderError> {
    let vendor = match adapter.vendor_id {
        0x10DE => "nvidia",
        0x8086 => "intel",
        0x1002 => "amd",
        _ => {
            return Err(preparation_error(
                "No MF AV1 backend is implemented for this adapter vendor.".into(),
            ));
        }
    };
    Ok(format!("{vendor}-mf-av1-d3d11:luid={:016X}", adapter.luid))
}

/// A real NVENC HEVC Main10 qualification on one adapter. This only verifies encoder
/// capability and configuration; it does not claim an HDR capture or conversion path.
#[derive(Clone, Debug)]
pub struct NativeHevcMain10Candidate {
    pub adapter: NativeAdapterIdentity,
    /// Driver-produced Annex-B VPS/SPS/PPS from the qualified session.
    pub codec_config: Vec<u8>,
    pub settings: NativeEncoderSettings,
}

pub fn probe_native_hevc_main10() -> Result<Vec<NativeHevcMain10Candidate>, RecorderError> {
    let config = NativeEncoderConfig::qualification()?;
    let (verified, _) = native::probe_hevc_adapters(&config)?;
    Ok(verified
        .into_iter()
        .map(|(adapter, settings)| NativeHevcMain10Candidate {
            adapter,
            codec_config: settings.codec_config.clone(),
            settings,
        })
        .collect())
}

/// Actual NVENC AV1 session qualification; a probe is not capture/HDR acceptance.
#[derive(Clone, Debug)]
pub struct NativeAv1Candidate {
    pub adapter: NativeAdapterIdentity,
    pub main10: bool,
    /// Driver-produced low-overhead sequence header OBUs.
    pub settings: NativeEncoderSettings,
}

pub fn probe_native_av1(main10: bool) -> Result<Vec<NativeAv1Candidate>, RecorderError> {
    probe_native_av1_diagnostics(main10).map(|(candidates, _)| candidates)
}

/// Every rejected adapter carries the actual native initialization reason.
pub fn probe_native_av1_diagnostics(
    main10: bool,
) -> Result<(Vec<NativeAv1Candidate>, Vec<String>), RecorderError> {
    let config = NativeEncoderConfig::qualification()?;
    let (verified, attempts) = native::probe_av1_adapters(&config, main10)?;
    let diagnostics = attempts
        .into_iter()
        .map(|attempt| format!("{}: {}", attempt.adapter, attempt.detail))
        .collect();
    Ok((
        verified
            .into_iter()
            .map(|(adapter, settings)| NativeAv1Candidate {
                adapter,
                main10,
                settings,
            })
            .collect(),
        diagnostics,
    ))
}

/// Discovers and qualifies the local NVIDIA hardware encoders.
///
/// Only candidates that completed a real qualification session — requested dimensions,
/// nominal rate, the accepted ADR 0006 configuration, ARGB input support and returned
/// initialization bytes — appear here. A vendor name alone is never availability, and a
/// missing or too-old driver is an explicit `EncoderUnavailable` error rather than an
/// empty list or a fallback.
pub fn probe_native_encoders() -> Result<Vec<NativeEncoderCandidate>, RecorderError> {
    let config = NativeEncoderConfig::qualification()?;
    let (verified, _attempts) = native::probe_adapters(&config)?;
    Ok(verified
        .into_iter()
        .map(|(adapter, settings)| NativeEncoderCandidate {
            capability: EncoderCapability {
                id: format!("nvidia-nvenc-h264-d3d11:luid={:016X}", adapter.luid),
                codec: "h264".to_string(),
                hardware: true,
            },
            adapter: adapter.description,
            adapter_luid: adapter.luid,
            settings,
        })
        .collect())
}

/// Reads the actual adapter of a captured texture. The native encoder uses this
/// same device; no adapter is inferred from a monitor name or probe order.
pub fn capture_adapter_identity(
    lease: &WindowsSurface,
) -> Result<NativeAdapterIdentity, RecorderError> {
    let texture = lease.encode_texture().ok_or_else(|| {
        preparation_error("The capture lease has no D3D11 texture backing.".to_string())
    })?;
    native::device_adapter(texture.device)
}

/// Adapter resolved from the exact output selection used by display capture.
/// A later first-lease LUID check still guards display topology changes.
pub fn display_capture_adapter_identity(
    display_id: &frame_contracts::DisplayId,
) -> Result<NativeAdapterIdentity, RecorderError> {
    crate::capture::native::display_adapter_identity(&display_id.0)
}

/// Adapter-scoped MF preparation from the exact capture lease. Results are
/// diagnostic and are never exposed by EncoderProbe as usable capabilities.
pub fn prepare_media_foundation_h264_for_surface(
    lease: &WindowsSurface,
    config: &NativeEncoderConfig,
) -> Result<Vec<MfH264Preparation>, RecorderError> {
    let identity = capture_adapter_identity(lease)?;
    prepare_hardware_h264(&identity, config)
}

/// Qualifies only encoders attached to this captured texture's adapter. A missing
/// candidate is an explicit no-hardware result, never a CPU or cross-GPU fallback.
pub fn probe_native_encoders_for_surface(
    lease: &WindowsSurface,
) -> Result<Vec<NativeEncoderCandidate>, RecorderError> {
    let identity = capture_adapter_identity(lease)?;
    Ok(probe_native_encoders()?
        .into_iter()
        .filter(|candidate| candidate.adapter_luid == identity.luid)
        .collect())
}

/// Qualifies the hardware encoder on the adapter selected by capture output.
/// MF requires a completed synthetic GPU frame and drain before publication.
pub fn probe_h264_encoder_for_adapter(
    adapter: &NativeAdapterIdentity,
    geometry: (u32, u32),
    nominal_rate: (NonZeroU32, NonZeroU32),
) -> Result<Vec<NativeEncoderCandidate>, RecorderError> {
    match adapter.vendor_id {
        0x10DE => Ok(probe_native_encoders()?
            .into_iter()
            .filter(|candidate| candidate.adapter_luid == adapter.luid)
            .collect()),
        0x8086 | 0x1002 => {
            let config =
                NativeEncoderConfig::proof(geometry.0, geometry.1, nominal_rate.0, nominal_rate.1)?;
            let settings = mf_process::qualify_adapter(adapter, &config)?;
            let provider = if adapter.vendor_id == 0x8086 {
                "intel"
            } else {
                "amd"
            };
            Ok(vec![NativeEncoderCandidate {
                capability: EncoderCapability {
                    id: format!("{provider}-mf-h264-d3d11:luid={:016X}", adapter.luid),
                    codec: "h264".into(),
                    hardware: true,
                },
                adapter: adapter.description.clone(),
                adapter_luid: adapter.luid,
                settings,
            }])
        }
        _ => Err(RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message: format!(
                "No hardware H.264 encoder is supported for capture vendor {:04X} on {}.",
                adapter.vendor_id, adapter.description
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Select a display on a supported Intel, AMD or NVIDIA adapter.".into(),
        }),
    }
}

/// One same-adapter synthetic GPU frame through MF H.264 and complete EOS.
/// Intended for a local file/decode diagnostic; it never opens display capture.
pub fn synthetic_media_foundation_h264_frame(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
) -> Result<(EncodedStreamInfo, EncodedPacket), RecorderError> {
    if !matches!(adapter.vendor_id, 0x8086 | 0x1002) {
        return Err(invalid_request_error(
            "The synthetic MF proof requires an Intel or AMD adapter.".into(),
        ));
    }
    media_foundation::synthetic_packet_encoder_frame(adapter, config)
}

/// Diagnostic sequence with bounded GPU leases; never captures a display or audio.
pub fn synthetic_media_foundation_h264_sequence(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    frame_count: u32,
    receive: impl FnMut(&EncodedStreamInfo, EncodedPacket) -> Result<(), RecorderError>,
) -> Result<(EncoderSessionStats, Option<MfTransferStats>), RecorderError> {
    synthetic_encoder_sequence(adapter, config, None, frame_count, || false, receive)
        .map(|result| (result.stats, result.transfer))
}

/// Short capture-free calibration through the same production packet encoder.
pub struct SyntheticEncoderSequence {
    pub settings: NativeEncoderSettings,
    pub qp: Option<NativeQpStatistics>,
    pub stats: EncoderSessionStats,
    pub transfer: Option<MfTransferStats>,
    pub latency_p95_ns: Option<u64>,
    pub overhead_p95_ns: Option<u64>,
    pub elapsed_ns: u64,
}

pub fn synthetic_encoder_sequence(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    selected_id: Option<&str>,
    frame_count: u32,
    cancelled: impl Fn() -> bool,
    receive: impl FnMut(&EncodedStreamInfo, EncodedPacket) -> Result<(), RecorderError>,
) -> Result<SyntheticEncoderSequence, RecorderError> {
    synthetic_encoder_sequence_with_pattern_period(
        adapter,
        config,
        selected_id,
        frame_count,
        1,
        cancelled,
        receive,
    )
}

/// Capture-free diagnostic with a bounded hold period for each generated pattern.
/// Holding detailed backgrounds permits temporal AQ behavior to be measured.
#[allow(clippy::too_many_arguments)]
pub fn synthetic_encoder_sequence_with_pattern_period(
    adapter: &NativeAdapterIdentity,
    config: &NativeEncoderConfig,
    selected_id: Option<&str>,
    frame_count: u32,
    pattern_period: u32,
    cancelled: impl Fn() -> bool,
    receive: impl FnMut(&EncodedStreamInfo, EncodedPacket) -> Result<(), RecorderError>,
) -> Result<SyntheticEncoderSequence, RecorderError> {
    media_foundation::synthetic_packet_encoder_sequence(
        adapter,
        config,
        selected_id,
        frame_count,
        pattern_period,
        cancelled,
        receive,
    )
}

/// Human-readable encoder discovery diagnostics for the read-only probe.
///
/// It reports the negotiated library/driver versions, the qualification configuration,
/// every adapter that was considered with the reason it was skipped or qualified, and
/// the effective configuration of each verified candidate. It creates no stream and
/// touches no capture session.
pub fn encode_diagnostics() -> Result<String, RecorderError> {
    let mut text = String::new();
    text.push_str("Frame Windows hardware encoder diagnostics (FRAME-0104)\n");
    text.push_str(
        "This is a hardware qualification report: it initializes and destroys a real encoder session and encodes nothing.\n",
    );
    let config = NativeEncoderConfig::qualification()?;
    text.push_str(&format!(
        "qualification request: {}x{} at {}/{} nominal, {} {} bit/s (peak {}), GOP {}, {}\n",
        config.width,
        config.height,
        config.rate_num,
        config.rate_den,
        config.rate_control.describe(),
        config.bitrate_bps,
        config.peak_bitrate_bps,
        config.gop_frames,
        config.preset.describe()
    ));
    match native::library_report() {
        (Some(info), detail) => {
            text.push_str(&format!("library: {}\n", info.describe()));
            text.push_str(&format!("library_detail: {detail}\n"));
        }
        (None, detail) => {
            text.push_str(&format!("library: unavailable: {detail}\n"));
            return Ok(text);
        }
    }
    let (verified, attempts) = native::probe_adapters(&config)?;
    text.push_str("adapters:\n");
    for attempt in &attempts {
        text.push_str(&format!(
            "  {} — {}: {}\n",
            attempt.adapter,
            if attempt.verified {
                "qualified"
            } else {
                "not qualified"
            },
            attempt.detail
        ));
    }
    if attempts.is_empty() {
        text.push_str("  (no DXGI adapter was reported)\n");
    }
    text.push_str(&format!("verified_candidates: {}\n", verified.len()));
    for (adapter, settings) in &verified {
        text.push_str(&format!(
            "  {} (LUID {:016X}): {}\n",
            adapter.description,
            adapter.luid,
            settings.describe()
        ));
    }
    text.push_str(
        "note: a discovery candidate is not a guarantee for every configuration; a usable session initializes the selected capture device for its actual dimensions and rate.\n",
    );
    Ok(text)
}

// ------------------------------------------------------------------- counters

/// Every bounded delivery counter of one encoder session.
#[derive(Default, Debug)]
struct EncoderCounters {
    accepted_inputs: AtomicU64,
    backpressure_returns: AtomicU64,
    rejected_inputs: AtomicU64,
    native_submissions: AtomicU64,
    /// Pictures the driver accepted and buffered (NEED_MORE_INPUT): outstanding work with
    /// an owned output slot, not a released input.
    native_buffered: AtomicU64,
    native_failures: AtomicU64,
    native_busy_retries: AtomicU64,
    /// Completed native pictures. Bumped by the same single producer that delivers the
    /// packet, so it counts frames and never access units.
    frames_completed: AtomicU64,
    packets_emitted: AtomicU64,
    packets_discarded_on_failure: AtomicU64,
    bytes_emitted: AtomicU64,
    packets_dequeued: AtomicU64,
    bytes_dequeued: AtomicU64,
    packets_discarded_on_stop: AtomicU64,
    inputs_discarded_on_stop: AtomicU64,
    slots_abandoned_at_stop: AtomicU64,
    inputs_discarded_on_failure: AtomicU64,
    slots_abandoned_on_failure: AtomicU64,
    inputs_discarded_on_unwind: AtomicU64,
    slots_abandoned_on_unwind: AtomicU64,
    /// Slots whose native use could not be confirmed finished, so they were not recycled.
    slots_kept_alive_on_failure: AtomicU64,
    /// Registrations kept alive because their release could not be confirmed.
    leases_kept_alive: AtomicU64,
    /// Native teardown steps that could not establish safe reuse.
    unsafe_cleanup_steps: AtomicU64,
    /// Retries of a native cleanup that could not confirm completion.
    unresolved_cleanup_retries: AtomicU64,
    /// 1 while a retained native owner exists (session, registrations, leases kept`n    /// because native completion was never confirmed).
    retained_native_owner: AtomicU64,
    registrations: AtomicU64,
    unregistrations: AtomicU64,
    eos_submissions: AtomicU64,
    eos_acknowledgements: AtomicU64,
    eos_busy_retries: AtomicU64,
    timestamp_mismatches: AtomicU64,
    timestamp_regressions: AtomicU64,
    consumer_contention: AtomicU64,
    drain_requests: AtomicU64,
    lease_high_water: AtomicU64,
    queue_high_water: AtomicU64,
    teardown_nanos: AtomicU64,
}

impl EncoderCounters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    fn raise(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    fn load(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> EncoderCounterSnapshot {
        EncoderCounterSnapshot {
            accepted_inputs: Self::load(&self.accepted_inputs),
            backpressure_returns: Self::load(&self.backpressure_returns),
            rejected_inputs: Self::load(&self.rejected_inputs),
            native_submissions: Self::load(&self.native_submissions),
            native_buffered: Self::load(&self.native_buffered),
            native_failures: Self::load(&self.native_failures),
            native_busy_retries: Self::load(&self.native_busy_retries),
            frames_completed: Self::load(&self.frames_completed),
            packets_emitted: Self::load(&self.packets_emitted),
            packets_discarded_on_failure: Self::load(&self.packets_discarded_on_failure),
            bytes_emitted: Self::load(&self.bytes_emitted),
            packets_dequeued: Self::load(&self.packets_dequeued),
            bytes_dequeued: Self::load(&self.bytes_dequeued),
            packets_discarded_on_stop: Self::load(&self.packets_discarded_on_stop),
            inputs_discarded_on_stop: Self::load(&self.inputs_discarded_on_stop),
            slots_abandoned_at_stop: Self::load(&self.slots_abandoned_at_stop),
            inputs_discarded_on_failure: Self::load(&self.inputs_discarded_on_failure),
            slots_abandoned_on_failure: Self::load(&self.slots_abandoned_on_failure),
            inputs_discarded_on_unwind: Self::load(&self.inputs_discarded_on_unwind),
            slots_abandoned_on_unwind: Self::load(&self.slots_abandoned_on_unwind),
            slots_kept_alive_on_failure: Self::load(&self.slots_kept_alive_on_failure),
            leases_kept_alive: Self::load(&self.leases_kept_alive),
            unsafe_cleanup_steps: Self::load(&self.unsafe_cleanup_steps),
            unresolved_cleanup_retries: Self::load(&self.unresolved_cleanup_retries),
            retained_native_owner: Self::load(&self.retained_native_owner) != 0,
            registrations: Self::load(&self.registrations),
            unregistrations: Self::load(&self.unregistrations),
            eos_submissions: Self::load(&self.eos_submissions),
            eos_acknowledgements: Self::load(&self.eos_acknowledgements),
            eos_busy_retries: Self::load(&self.eos_busy_retries),
            timestamp_mismatches: Self::load(&self.timestamp_mismatches),
            timestamp_regressions: Self::load(&self.timestamp_regressions),
            consumer_contention: Self::load(&self.consumer_contention),
            drain_requests: Self::load(&self.drain_requests),
            lease_high_water: Self::load(&self.lease_high_water),
            queue_high_water: Self::load(&self.queue_high_water),
            teardown_nanos: Self::load(&self.teardown_nanos),
            packets_held: 0,
            bytes_held: 0,
            retained_leases: 0,
            queued_inputs: 0,
            inflight: 0,
        }
    }
}

/// One immutable snapshot of the encoder's bounded delivery counters.
///
/// It is a live snapshot, not an atomic global view: only a post-`stop` snapshot is
/// expected to reconcile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderCounterSnapshot {
    pub accepted_inputs: u64,
    pub backpressure_returns: u64,
    pub rejected_inputs: u64,
    pub native_submissions: u64,
    /// Pictures the driver accepted and buffered; they are outstanding until collected.
    pub native_buffered: u64,
    pub native_failures: u64,
    pub native_busy_retries: u64,
    /// Native pictures the driver completed. One completed picture produces exactly one
    /// delivered packet on this session, and the equality is checked by
    /// [`EncoderCounterSnapshot::identities`] rather than assumed.
    pub frames_completed: u64,
    pub packets_emitted: u64,
    pub packets_discarded_on_failure: u64,
    pub bytes_emitted: u64,
    pub packets_dequeued: u64,
    pub bytes_dequeued: u64,
    pub packets_discarded_on_stop: u64,
    pub inputs_discarded_on_stop: u64,
    pub slots_abandoned_at_stop: u64,
    pub inputs_discarded_on_failure: u64,
    pub slots_abandoned_on_failure: u64,
    pub inputs_discarded_on_unwind: u64,
    pub slots_abandoned_on_unwind: u64,
    /// Slots deliberately not recycled because native use was not proven finished.
    pub slots_kept_alive_on_failure: u64,
    /// Registrations deliberately kept alive for the same reason.
    pub leases_kept_alive: u64,
    /// Teardown steps that could not establish safe reuse.
    pub unsafe_cleanup_steps: u64,
    /// Retries of a native cleanup that could not confirm completion.
    pub unresolved_cleanup_retries: u64,
    /// Whether a bounded retained native owner is held by this session.
    pub retained_native_owner: bool,
    pub registrations: u64,
    pub unregistrations: u64,
    pub eos_submissions: u64,
    pub eos_acknowledgements: u64,
    pub eos_busy_retries: u64,
    pub timestamp_mismatches: u64,
    pub timestamp_regressions: u64,
    pub consumer_contention: u64,
    pub drain_requests: u64,
    pub lease_high_water: u64,
    pub queue_high_water: u64,
    pub teardown_nanos: u64,
    /// Packets still owned by the encoder at the moment of the snapshot.
    pub packets_held: u64,
    /// Compressed bytes still owned by the encoder at the moment of the snapshot.
    pub bytes_held: u64,
    /// Capture leases still retained at the moment of the snapshot.
    pub retained_leases: u64,
    /// Accepted inputs still waiting to be submitted natively.
    pub queued_inputs: u64,
    /// Native submissions whose output buffer has not been collected yet.
    pub inflight: u64,
}

impl EncoderCounterSnapshot {
    fn lines(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("accepted_inputs", self.accepted_inputs),
            ("backpressure_returns", self.backpressure_returns),
            ("rejected_inputs", self.rejected_inputs),
            ("native_submissions", self.native_submissions),
            ("native_buffered", self.native_buffered),
            ("native_failures", self.native_failures),
            ("native_busy_retries", self.native_busy_retries),
            ("frames_completed", self.frames_completed),
            ("packets_emitted", self.packets_emitted),
            (
                "packets_discarded_on_failure",
                self.packets_discarded_on_failure,
            ),
            ("bytes_emitted", self.bytes_emitted),
            ("packets_dequeued", self.packets_dequeued),
            ("bytes_dequeued", self.bytes_dequeued),
            ("packets_discarded_on_stop", self.packets_discarded_on_stop),
            ("inputs_discarded_on_stop", self.inputs_discarded_on_stop),
            ("slots_abandoned_at_stop", self.slots_abandoned_at_stop),
            (
                "inputs_discarded_on_failure",
                self.inputs_discarded_on_failure,
            ),
            (
                "slots_abandoned_on_failure",
                self.slots_abandoned_on_failure,
            ),
            (
                "inputs_discarded_on_unwind",
                self.inputs_discarded_on_unwind,
            ),
            ("slots_abandoned_on_unwind", self.slots_abandoned_on_unwind),
            ("registrations", self.registrations),
            ("unregistrations", self.unregistrations),
            ("eos_submissions", self.eos_submissions),
            ("eos_acknowledgements", self.eos_acknowledgements),
            ("eos_busy_retries", self.eos_busy_retries),
            ("timestamp_mismatches", self.timestamp_mismatches),
            ("timestamp_regressions", self.timestamp_regressions),
            ("consumer_contention", self.consumer_contention),
            ("drain_requests", self.drain_requests),
            ("lease_high_water", self.lease_high_water),
            ("queue_high_water", self.queue_high_water),
            ("teardown_nanos", self.teardown_nanos),
            ("packets_held", self.packets_held),
            ("bytes_held", self.bytes_held),
            ("retained_leases", self.retained_leases),
            ("queued_inputs", self.queued_inputs),
            ("submitted_in_flight", self.submitted_in_flight()),
        ]
    }

    /// Native submissions whose output buffer has not been collected yet.
    pub fn submitted_in_flight(&self) -> u64 {
        self.inflight
    }

    /// Re-checks the documented accounting identities.
    ///
    /// Every accepted input is either submitted natively, consumed without output,
    /// failed, rejected by policy, or explicitly discarded by a stop, a native failure or
    /// an unwind; every native submission is either delivered, discarded on failure, or
    /// abandoned by a stop, a failure or an unwind; every emitted packet is either
    /// dequeued, discarded by stop, or still held. An `Err` names the first identity that
    /// does not hold.
    pub fn identities(&self) -> Result<(), String> {
        let inputs = self
            .native_submissions
            .saturating_add(self.native_buffered)
            .saturating_add(self.native_failures)
            .saturating_add(self.rejected_inputs)
            .saturating_add(self.inputs_discarded_on_stop)
            .saturating_add(self.inputs_discarded_on_failure)
            .saturating_add(self.inputs_discarded_on_unwind)
            .saturating_add(self.queued_inputs);
        if inputs != self.accepted_inputs {
            return Err(format!(
                "accepted_inputs={} but native_submissions+buffered+failures+rejected+discarded+queued={inputs}",
                self.accepted_inputs
            ));
        }
        if self.retained_leases
            != self
                .queued_inputs
                .saturating_add(self.submitted_in_flight())
        {
            return Err(format!(
                "retained_leases={} but queued_inputs+submitted_in_flight={}",
                self.retained_leases,
                self.queued_inputs
                    .saturating_add(self.submitted_in_flight())
            ));
        }
        let accepted_by_driver = self.native_submissions.saturating_add(self.native_buffered);
        let submissions = self
            .packets_emitted
            .saturating_add(self.packets_discarded_on_failure)
            .saturating_add(self.slots_abandoned_at_stop)
            .saturating_add(self.slots_abandoned_on_failure)
            .saturating_add(self.slots_abandoned_on_unwind)
            .saturating_add(self.submitted_in_flight());
        if submissions != accepted_by_driver {
            return Err(format!(
                "native_submissions+buffered={accepted_by_driver} but emitted+discarded_on_failure+abandoned+in_flight={submissions}"
            ));
        }
        let delivered = self
            .packets_dequeued
            .saturating_add(self.packets_discarded_on_stop)
            .saturating_add(self.packets_held);
        if delivered != self.packets_emitted {
            return Err(format!(
                "packets_emitted={} but dequeued+discarded_on_stop+held={delivered}",
                self.packets_emitted
            ));
        }
        // A packet is only a frame while this holds: one completed native picture must
        // deliver exactly one access unit. It is checked here, so an encoded rate may be
        // measured on the frame counter instead of assumed from the packet counter.
        if self.frames_completed != self.packets_emitted {
            return Err(format!(
                "frames_completed={} but packets_emitted={}: a completed native picture did not deliver exactly one access unit",
                self.frames_completed, self.packets_emitted
            ));
        }
        if self.packets_held > MAX_PACKETS as u64 {
            return Err(format!(
                "packets_held={} exceeds the bounded {MAX_PACKETS}",
                self.packets_held
            ));
        }
        if self.bytes_held > MAX_PACKET_BYTES {
            return Err(format!(
                "bytes_held={} exceeds the bounded {MAX_PACKET_BYTES}",
                self.bytes_held
            ));
        }
        if self.retained_leases > MAX_ENCODER_LEASES as u64 {
            return Err(format!(
                "retained_leases={} exceeds the bounded {MAX_ENCODER_LEASES}",
                self.retained_leases
            ));
        }
        Ok(())
    }
}

// ------------------------------------------------------------ shared session

/// One read-only snapshot of an encoder session's counters and bounded gauges
/// (FRAME-0203).
///
/// This is the data path beside the human-readable `diagnostics()` text: a consumer reads
/// typed numbers, never a report string. It is a live snapshot of separate atomics, not
/// one atomic global snapshot, so only a post-`stop` snapshot is expected to reconcile;
/// it allocates nothing and never waits for the worker, so a UI may sample it on its own
/// clock and turn two samples into a measured rate.
///
/// [`Self::completed_frames`] is the frame counter an encoded FPS may be measured on.
/// [`Self::packets_emitted`] is the delivered access-unit counter; a packet is a frame only
/// while [`Self::packets_match_frames`] holds, and that equality is checked by the
/// session's own accounting identities rather than assumed. The unknown/contended values
/// of a running session (queue depth, in-flight work) are reported as the counters that
/// really exist; nothing is estimated from wall-clock time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderSessionStats {
    /// Inputs this session admitted. Never fabricated and never duplicated.
    pub accepted_inputs: u64,
    /// Inputs returned to the caller because a bounded credit was exhausted.
    pub backpressure_returns: u64,
    /// Inputs refused by policy before admission.
    pub rejected_inputs: u64,
    /// Completed native pictures: the encoded frame count of this session.
    pub completed_frames: u64,
    /// Delivered Annex-B access units.
    pub packets_emitted: u64,
    /// Delivered access units the consumer has already taken.
    pub packets_dequeued: u64,
    /// Delivered access units the session still holds.
    pub packets_held: u64,
    /// The bounded delivery capacity in packets.
    pub packet_capacity: u64,
    /// Accepted inputs still waiting to be submitted natively (the ingress depth).
    pub queued_inputs: u64,
    /// The bounded ingress capacity in inputs.
    pub ingress_capacity: u64,
    /// The highest ingress depth this session observed.
    pub queue_high_water: u64,
    /// Native submissions whose output has not been collected yet.
    pub submitted_in_flight: u64,
    /// Capture leases this session retains right now.
    pub retained_leases: u64,
    /// The bounded lease capacity.
    pub lease_capacity: u64,
    /// Native submissions that failed.
    pub native_failures: u64,
    /// Inputs discarded by an explicit stop.
    pub discarded_on_stop: u64,
    /// Inputs discarded by a native failure.
    pub discarded_on_failure: u64,
    /// Inputs discarded by a worker unwind.
    pub discarded_on_unwind: u64,
}

impl EncoderSessionStats {
    /// Whether one completed native picture delivered exactly one access unit.
    ///
    /// `false` means the packet count and the frame count disagree, so an encoded FPS must
    /// not be measured from either. The session's own `identities` reports the same
    /// equality; it is re-checked here so a rate consumer never has to trust the text.
    pub fn packets_match_frames(&self) -> bool {
        self.packets_emitted == self.completed_frames
    }
}

// ------------------------------------------------------------ shared session

/// The bounded packet queue. It is never cloned, and it never grows past
/// [`MAX_PACKETS`] packets or [`MAX_PACKET_BYTES`] bytes of compressed data.
#[derive(Default)]
struct PacketQueue {
    packets: VecDeque<EncodedPacket>,
    bytes: u64,
}

impl PacketQueue {
    fn push(&mut self, packet: EncodedPacket) -> Result<(), EncodedPacket> {
        let length = packet.data.capacity() as u64;
        if self.packets.len() >= MAX_PACKETS || self.bytes.saturating_add(length) > MAX_PACKET_BYTES
        {
            return Err(packet);
        }
        self.bytes += length;
        self.packets.push_back(packet);
        Ok(())
    }

    fn pop(&mut self) -> Option<EncodedPacket> {
        let packet = self.packets.pop_front()?;
        self.bytes = self.bytes.saturating_sub(packet.data.capacity() as u64);
        Some(packet)
    }

    fn drain(&mut self) -> Vec<EncodedPacket> {
        self.bytes = 0;
        self.packets.drain(..).collect()
    }
}

/// The worker's terminal outcome, published exactly once after all native cleanup.
#[derive(Debug)]
enum TerminalOutcome {
    /// Cleanup finished; `drained` records whether the stream was completed.
    Clean { drained: bool },
    /// The session failed, or its cleanup could not establish safe reuse.
    Failed(RecorderError),
}

/// Per-session state shared by the worker, the control thread and the consumer.
struct EncoderShared {
    replay_keyframes: AtomicBool,
    owner_wake: OnceLock<frame_capture::PipelineWake>,
    worker_wake: OnceLock<std::thread::Thread>,
    /// Published after every cleanup step, so a consumer can never observe a successful
    /// drain while a registration or the session is still being released. `OnceLock`
    /// provides the happens-before edge; the consumer reads this first.
    outcome: OnceLock<TerminalOutcome>,
    terminal: OnceLock<RecorderError>,
    /// Set only by a failed native operation/progress check, never by an owner,
    /// queue, input-contract or panic error. Restart admission reads both fields.
    native_fault: AtomicBool,
    worker_finished: AtomicBool,
    eos_acknowledged: AtomicBool,
    retained: AtomicUsize,
    /// Per-stage visit counters, so a parent-process watchdog can see which stage the
    /// worker is spinning in. Diagnostic only.
    stage_visits: [AtomicU64; 10],
    /// Bounded worker-stage code, so a parent-process watchdog can see where the worker is
    /// without any native call being in flight. Diagnostic only.
    worker_stage: AtomicU64,
    /// Native submissions whose output buffer has not been collected or abandoned yet.
    /// It is a gauge maintained by the worker, never derived from the counters.
    inflight: AtomicUsize,
    /// Accepted inputs that are neither submitted natively nor released yet: they are in
    /// the ingress channel or in the worker's own queue. They make the accounting
    /// identity true at every instant, not only after stop.
    queued_inputs: AtomicUsize,
    packets_held: AtomicUsize,
    bytes_held: AtomicU64,
    counters: EncoderCounters,
    packets: Mutex<PacketQueue>,
    native_stats: Mutex<NativeStats>,
    mf_transfer_stats: Mutex<Option<MfTransferStats>>,
    #[cfg(test)]
    before_eos_check: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Native picture evidence for the last delivered access unit: the driver's own
    /// `NV_ENC_PIC_TYPE` value and frame index.
    native_last_picture: Mutex<Option<(u32, u64)>>,
    /// The last native call phase observed, taken before native cleanup so a call that
    /// never returned is named from evidence.
    last_native_phase: Mutex<String>,
}

impl EncoderShared {
    fn new() -> Self {
        Self {
            replay_keyframes: AtomicBool::new(false),
            owner_wake: OnceLock::new(),
            worker_wake: OnceLock::new(),
            outcome: OnceLock::new(),
            terminal: OnceLock::new(),
            native_fault: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            eos_acknowledged: AtomicBool::new(false),
            retained: AtomicUsize::new(0),
            worker_stage: AtomicU64::new(0),
            stage_visits: std::array::from_fn(|_| AtomicU64::new(0)),
            inflight: AtomicUsize::new(0),
            queued_inputs: AtomicUsize::new(0),
            packets_held: AtomicUsize::new(0),
            bytes_held: AtomicU64::new(0),
            counters: EncoderCounters::default(),
            packets: Mutex::new(PacketQueue::default()),
            native_stats: Mutex::new(NativeStats::default()),
            mf_transfer_stats: Mutex::new(None),
            #[cfg(test)]
            before_eos_check: Mutex::new(None),
            native_last_picture: Mutex::new(None),
            last_native_phase: Mutex::new("no native call observed".to_string()),
        }
    }

    fn notify_owner(&self) {
        if let Some(wake) = self.owner_wake.get() {
            wake.notify();
        }
    }

    fn notify_worker(&self) {
        if let Some(worker) = self.worker_wake.get() {
            worker.unpark();
        }
    }

    /// Records the driver's current native call phase, for failure evidence.
    fn record_native_phase(&self, driver: &dyn NvencDriver) {
        let phase = driver.phase();
        if let Ok(mut slot) = self.last_native_phase.lock() {
            *slot = phase;
        }
    }

    fn set_stage(&self, stage: u64) {
        self.worker_stage.store(stage, Ordering::Relaxed);
        if let Some(counter) = self.stage_visits.get(stage as usize) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Bounded per-stage visit counts, for the watchdog trace.
    fn stage_counts(&self) -> String {
        self.stage_visits
            .iter()
            .enumerate()
            .filter(|(_, counter)| counter.load(Ordering::Relaxed) > 0)
            .map(|(index, counter)| format!("{index}:{}", counter.load(Ordering::Relaxed)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn stage(&self) -> &'static str {
        match self.worker_stage.load(Ordering::Relaxed) {
            1 => "polling completions",
            2 => "receiving jobs",
            3 => "submitting queued jobs",
            4 => "waiting for a packet credit",
            5 => "waiting for end of stream",
            6 => "idle wait for a job",
            7 => "handing an input to the driver",
            11 => "inside NvencDriver::submit",
            12 => "returned from NvencDriver::submit",
            13 => "inside poll_completions",
            14 => "inside NvencDriver::collect",
            15 => "after complete()",
            8 => "cleaning up",
            9 => "finished",
            _ => "starting",
        }
    }

    fn native_stats_summary(&self) -> String {
        let stats = self
            .native_stats
            .lock()
            .map(|stats| *stats)
            .unwrap_or_default();
        format!(
            "native_calls={} busy={} pending_inputs={}",
            stats.native_calls, stats.busy_results, stats.pending_inputs
        )
    }

    /// A bounded description of what a failed cleanup retained, or `None`.
    ///
    /// It reads the retained session (through its own phase record), its registrations and
    /// its leases, so retention is observable instead of being an invisible leak.
    fn retained_summary(&self) -> Option<String> {
        self.counters
            .retained_native_owner
            .load(Ordering::Acquire)
            .ne(&0)
            .then(|| {
                "native cleanup unresolved; the original joined worker owns the complete graph"
                    .to_string()
            })
    }

    fn native_phase(&self) -> String {
        self.last_native_phase
            .lock()
            .map(|phase| phase.clone())
            .unwrap_or_else(|poison| poison.into_inner().clone())
    }

    /// Publishes the one sticky terminal error; later attempts keep the first.
    fn fail(&self, error: RecorderError) {
        let _ = self.terminal.set(error);
        self.notify_owner();
    }

    fn native_error(&self, error: RecorderError) -> RecorderError {
        self.native_fault.store(true, Ordering::Release);
        error
    }

    fn mark_worker_finished(&self) {
        self.worker_finished.store(true, Ordering::Release);
        self.notify_owner();
    }

    /// Publishes the terminal outcome. Called once, after every native cleanup step.
    fn publish(&self, outcome: TerminalOutcome) {
        let _ = self.outcome.set(outcome);
    }

    /// The terminal outcome, if the worker has finished.
    fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.get()
    }

    /// True only when cleanup completed *and* the stream was drained with every accepted
    /// input accounted for and the delivery queue empty.
    fn drain_reached(&self) -> bool {
        matches!(
            self.terminal_outcome(),
            Some(TerminalOutcome::Clean { drained: true })
        ) && self.retained.load(Ordering::Acquire) == 0
            && self.packets_held.load(Ordering::Acquire) == 0
    }

    fn push_packet(&self, packet: EncodedPacket) -> Result<(), RecorderError> {
        let length = packet.data.capacity() as u64;
        let mut queue = self.packets.lock().map_err(|_| {
            internal_error(
                "The encoder packet queue was poisoned by a panic, so it cannot accept a packet."
                    .to_string(),
            )
        })?;
        if queue.push(packet).is_err() {
            return Err(internal_error(format!(
                "The encoder packet queue refused a {length}-byte packet, which would exceed the bounded {MAX_PACKETS} packets or {MAX_PACKET_BYTES} bytes."
            )));
        }
        let held = self.packets_held.fetch_add(1, Ordering::AcqRel) + 1;
        self.bytes_held.fetch_add(length, Ordering::AcqRel);
        EncoderCounters::raise(&self.counters.queue_high_water, held as u64);
        drop(queue);
        self.notify_owner();
        Ok(())
    }
}

/// Out-of-band control flags: neither drain nor stop needs a lock or a queue slot.
#[derive(Default)]
struct EncodeControl {
    drain: AtomicBool,
    stop: AtomicBool,
}

// --------------------------------------------------------------------- worker

/// One accepted input on its way to the driver.
struct Job {
    frame: CapturedFrame<WindowsSurface>,
    pts: MediaTimestamp,
    duration: NonZeroU64,
}

/// Combines the primary failure with the cleanup failure, keeping both.
///
/// A cleanup error is context for the primary failure, never a replacement: dropping it
/// would hide half of what happened during teardown.
fn combine_cleanup_errors(
    primary: Option<RecorderError>,
    cleanup: Option<RecorderError>,
) -> RecorderError {
    match (primary, cleanup) {
        (Some(primary), Some(cleanup)) => RecorderError {
            code: primary.code,
            message: format!(
                "{} Cleanup also reported: {}",
                primary.message, cleanup.message
            ),
            recovery: primary.recovery,
            action: format!("{} Then: {}", primary.action, cleanup.action),
        },
        (Some(primary), None) => primary,
        (None, Some(cleanup)) => cleanup,
        (None, None) => {
            internal_error("Encoder cleanup failed without a reported error.".to_string())
        }
    }
}

/// The whole-picture packet-memory bound, in one place.
///
/// Packet count and owned bytes are separate bounds, and the byte bound includes the
/// driver's permanent collection buffer, so four maximum-size queued packets plus that
/// buffer can never exceed [`MAX_PACKET_BYTES`].
fn held_credits_available(
    held_packets: u64,
    held_bytes: u64,
    inflight: u64,
    reserved_bytes: u64,
    new_reservation: u64,
) -> bool {
    held_packets.saturating_add(inflight) < MAX_PACKETS as u64
        && held_bytes
            .saturating_add(reserved_bytes)
            .saturating_add(new_reservation)
            .saturating_add(crate::encode::native::SCRATCH_BYTES)
            <= MAX_PACKET_BYTES
}

/// The result of offering one accepted input to the driver.
enum SubmitAttempt {
    /// The driver took the input, or consumed it without producing a bitstream.
    Submitted,
    /// The driver was busy; the unchanged input must be retried.
    Retry(Job),
}

/// Why a worker stopped owning leases without finishing them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Discard {
    Failure,
    Unwind,
}

/// One native submission whose output buffer has not completed yet. It owns the capture
/// lease until native completion and input unmap have both happened.
struct InFlight {
    slot: u32,
    lease: CapturedFrame<WindowsSurface>,
    pts: u64,
    duration: u64,
    reserved: u64,
}

/// One cached native texture registration.
///
/// It owns the private backing payload of the texture it registered, so the device and
/// that texture stay alive through initialization, idle periods and cleanup even when no
/// frame lease is outstanding, and they are only dropped after the registration has been
/// released natively (or after the session itself has been destroyed).
struct Registration {
    texture: usize,
    index: u32,
    _backing: SharedPayload,
}

/// Owns the worker thread from the moment it is spawned.
///
/// While preparation is in flight the handle must never be dropped: dropping it would
/// detach a live worker whose native preparation call may still be running. This guard
/// signals stop on every failure path and then joins, so ownership is retained instead of
/// detached, and the worker's own guard remains the only path that closes the session.
struct JoinGuard {
    handle: Option<std::thread::JoinHandle<()>>,
    control: Arc<EncodeControl>,
    joined: bool,
}

impl JoinGuard {
    fn new(handle: std::thread::JoinHandle<()>, control: Arc<EncodeControl>) -> Self {
        Self {
            handle: Some(handle),
            control,
            joined: false,
        }
    }

    /// Signals cancellation and joins, returning whether the worker joined cleanly.
    ///
    /// A preparation call that cannot be interrupted makes this block; that is failed-run
    /// evidence, not a licence to detach the thread or to fake cleanup.
    fn stop_and_join(&mut self) -> bool {
        if self.joined {
            return true;
        }
        self.joined = true;
        self.control.stop.store(true, Ordering::Release);
        match self.handle.take() {
            Some(handle) => handle.join().is_ok(),
            None => true,
        }
    }
}

impl Drop for JoinGuard {
    fn drop(&mut self) {
        if !self.joined {
            // Retained ownership rather than a detached worker: signal stop and join.
            let _ = self.stop_and_join();
        }
    }
}

/// Owns a driver only while the facade is being assembled, so a refused configuration
/// still closes the native session exactly once.
struct OpenGuard {
    driver: Option<Box<dyn NvencDriver>>,
}

impl OpenGuard {
    fn new(driver: Box<dyn NvencDriver>) -> Self {
        Self {
            driver: Some(driver),
        }
    }

    fn driver(&mut self) -> &mut dyn NvencDriver {
        match self.driver.as_mut() {
            Some(driver) => driver.as_mut(),
            None => unreachable!("the open guard owns its driver until it is taken"),
        }
    }

    /// Hands the driver to the worker and disarms the guard.
    fn take(mut self) -> Box<dyn NvencDriver> {
        match self.driver.take() {
            Some(driver) => driver,
            None => unreachable!("the open guard owns its driver until it is taken"),
        }
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(mut driver) = self.driver.take() {
            loop {
                let _ = driver.close();
                if !driver.cleanup_unsafe() {
                    break;
                }
                std::thread::sleep(UNCLEAN_RETRY_INTERVAL);
            }
        }
    }
}

/// Owns the driver so its session is released exactly once on any exit path, including
/// an unwind.
struct DriverGuard {
    driver: Box<dyn NvencDriver>,
    shared: Arc<EncoderShared>,
    closed: bool,
}

impl DriverGuard {
    fn new(driver: Box<dyn NvencDriver>, shared: Arc<EncoderShared>) -> Self {
        Self {
            driver,
            shared,
            closed: false,
        }
    }

    fn finish(&mut self) -> Result<(), RecorderError> {
        if self.closed {
            return Ok(());
        }
        let stats = self.driver.stats();
        if let Ok(mut slot) = self.shared.native_stats.lock() {
            *slot = stats;
        }
        if let Some(stats) = self.driver.mf_transfer_stats()
            && let Ok(mut slot) = self.shared.mf_transfer_stats.lock()
        {
            *slot = Some(stats);
        }
        let mut failure = None;
        loop {
            let result = self.driver.close();
            if let Err(error) = result {
                self.shared.fail(error.clone());
                failure.get_or_insert(error);
            }
            if !self.driver.cleanup_unsafe() {
                self.closed = true;
                self.shared
                    .counters
                    .retained_native_owner
                    .store(0, Ordering::Release);
                return failure.map_or(Ok(()), Err);
            }
            // Keep the actual driver, backing, registrations and leases on this owner
            // thread. No transfer to facade lifetime and no lock across the native call.
            EncoderCounters::bump(&self.shared.counters.unsafe_cleanup_steps);
            self.shared
                .counters
                .retained_native_owner
                .store(1, Ordering::Release);
            self.shared.fail(internal_error("Native cleanup remains unresolved; the owner worker is retaining its resources until safe release.".to_string()));
            std::thread::sleep(UNCLEAN_RETRY_INTERVAL);
            EncoderCounters::bump(&self.shared.counters.unresolved_cleanup_retries);
        }
    }
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The single encoder worker.
struct Worker {
    shared: Arc<EncoderShared>,
    control: Arc<EncodeControl>,
    driver: DriverGuard,
    ingress: Receiver<Job>,
    /// The capture device this session was opened on; any other device is rejected.
    expected_device: usize,
    /// The capture pool this session was opened for; any other pool is rejected.
    expected_pool: u64,
    settings: NativeEncoderSettings,
    /// Retains the capture device and its textures independently of any frame lease, so
    /// the session's device stays alive through initialization, idle periods and cleanup
    /// even when no frame is in flight.
    _backing: SharedPayload,
    inflight: Vec<InFlight>,
    registrations: Vec<Registration>,
    local: VecDeque<Job>,
    /// The one input currently inside a driver call, so an unwind still accounts for it.
    handing_over: Option<Job>,
    force_idr: bool,
    last_replay_idr_pts: Option<u64>,
    last_emitted_pts: Option<u64>,
    reserved_bytes: u64,
    eos_submitted: bool,
    eos_started: Option<Instant>,
    channel_empty: bool,
    busy_since: Option<Instant>,
    /// When the oldest in-flight native submission started, so a driver that stops
    /// completing work is bounded by the five-second progress watchdog.
    oldest_submission: Option<Instant>,
    /// Set once this worker has accounted for everything it owned, so the same lease can
    /// never be accounted twice.
    finished: bool,
    /// Loop counter for bounded diagnostic phase sampling.
    iterations: u64,
}

impl Worker {
    fn run(&mut self) -> Result<(), RecorderError> {
        let outcome = self.run_loop();
        if outcome.is_err() {
            // A native or session failure still has to account for every input it was
            // retaining and every submission it abandoned; nothing may be dropped
            // silently just because the session is failing.
            self.abandon_on_failure();
        }
        outcome
    }

    /// Accounts for everything this worker still owns after a terminal failure.
    ///
    /// An input that reached the driver is already accounted for by `native_submissions`,
    /// so only the abandoned output slots are counted for those; an input that never
    /// reached the driver is counted as a discarded input.
    fn abandon_on_failure(&mut self) {
        self.account_abandoned(Discard::Failure);
    }

    /// The same accounting for a worker that unwound. It is called explicitly once the
    /// panic has been caught at the thread boundary, and again (idempotently) from
    /// `Drop` as the safety net.
    fn abandon_after_unwind(&mut self) {
        self.account_abandoned(Discard::Unwind);
    }

    fn account_abandoned(&mut self, kind: Discard) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Native cleanup first: destroying the session unmaps every input and frees every
        // output buffer, so a released lease cannot hand a texture the driver still reads
        // back to the capture pool. The phase is recorded before that cleanup so the exact
        // unfinished call survives the teardown.
        self.shared.record_native_phase(self.driver.driver.as_ref());
        let _ = self.release_registrations();
        let _ = self.close_driver();
        let abandoned: Vec<InFlight> = self.inflight.drain(..).collect();
        if !abandoned.is_empty() {
            self.shared
                .inflight
                .fetch_sub(abandoned.len(), Ordering::AcqRel);
        }
        for entry in abandoned {
            match kind {
                Discard::Failure => {
                    EncoderCounters::bump(&self.shared.counters.slots_abandoned_on_failure);
                }
                Discard::Unwind => {
                    EncoderCounters::bump(&self.shared.counters.slots_abandoned_on_unwind);
                }
            }
            self.release_reservation(entry.reserved);
            self.release_lease(entry.lease);
        }
        let discard_input = |worker: &Self, job: Job| {
            match kind {
                Discard::Failure => {
                    EncoderCounters::bump(&worker.shared.counters.inputs_discarded_on_failure);
                }
                Discard::Unwind => {
                    EncoderCounters::bump(&worker.shared.counters.inputs_discarded_on_unwind);
                }
            }
            worker.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            worker.release_lease(job.frame);
        };
        if let Some(job) = self.handing_over.take() {
            discard_input(self, job);
        }
        while let Some(job) = self.local.pop_front() {
            discard_input(self, job);
        }
        while let Ok(job) = self.ingress.try_recv() {
            discard_input(self, job);
        }
    }

    fn run_loop(&mut self) -> Result<(), RecorderError> {
        let _ = self.shared.worker_wake.set(std::thread::current());
        loop {
            self.iterations = self.iterations.wrapping_add(1);
            // Bounded live phase sampling: a native call that never returns stays visible
            // in diagnostics and in a parent-process watchdog trace.
            if self.iterations % 32 == 1 {
                self.shared.record_native_phase(self.driver.driver.as_ref());
            }
            self.shared.set_stage(1);
            self.poll_completions(0)?;
            if self.control.stop.load(Ordering::Acquire) {
                self.shared.set_stage(8);
                return self.shutdown_early();
            }

            // Take whatever the caller queued into this worker's own bounded queue. The
            // channel cannot deliver more than the lease bound. An empty observation
            // can become stale before drain is published; the queued-input gauge
            // below prevents EOS from overtaking that final accepted input.
            self.shared.set_stage(2);
            self.channel_empty = false;
            while self.local.len() < INGRESS_CAPACITY {
                match self.ingress.try_recv() {
                    Ok(job) => self.local.push_back(job),
                    Err(TryRecvError::Empty) => {
                        self.channel_empty = true;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.channel_empty = true;
                        break;
                    }
                }
            }

            self.shared.set_stage(3);
            self.submit_local()?;

            #[cfg(test)]
            if self.local.is_empty() && self.channel_empty {
                let hook = self.shared.before_eos_check.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }

            // EOS is signalled once every accepted input has reached NVENC, even when
            // outputs (including buffered pictures) are still outstanding: requiring an
            // empty in-flight set would leave buffered input unflushed forever.
            if self.control.drain.load(Ordering::Acquire)
                && !self.eos_submitted
                && self.local.is_empty()
                && self.channel_empty
                // Drain's Acquire observes prior admission. A caller increments this
                // gauge before publishing a job, even if Empty was just observed.
                && self.shared.queued_inputs.load(Ordering::Acquire) == 0
            {
                self.submit_eos()?;
            }

            if self.eos_submitted {
                self.poll_completions(NATIVE_WAIT_MS)?;
                let acknowledged = self.poll_eos()?;
                // An acknowledged end of stream only completes the drain once every
                // outstanding picture (including buffered ones the driver still owes) has
                // been collected; until then the progress and drain watchdogs stay armed,
                // so a picture that never arrives is an explicit bounded failure.
                if acknowledged && self.inflight.is_empty() {
                    return self.finish_drain();
                }
                self.shared.set_stage(5);
                self.check_progress_watchdog()?;
                self.check_drain_watchdog()?;
                continue;
            }

            // Fill the hardware before waiting for output. A buffered encoder
            // may require later input to complete its oldest picture.
            if self.inflight.is_empty() {
                self.shared.set_stage(6);
                std::thread::park_timeout(IDLE_POLL);
            } else {
                self.poll_completions(NATIVE_WAIT_MS)?;
            }
            self.check_progress_watchdog()?;
        }
    }

    /// Records the last native picture evidence reported by the driver.
    fn record_native_picture(&self, picture_type: u32, frame_index: u64) {
        if let Ok(mut evidence) = self.shared.native_last_picture.lock() {
            *evidence = Some((picture_type, frame_index));
        }
    }

    /// Ordered nonblocking collection, optionally followed by one bounded wait.
    /// No B frames are configured; a later ready slot must not overtake the oldest.
    fn poll_completions(&mut self, wait_ms: u32) -> Result<(), RecorderError> {
        if self.inflight.is_empty() {
            return Ok(());
        }
        self.shared.set_stage(13);
        while let Some(first) = self.inflight.first() {
            let slot = first.slot;
            self.shared.set_stage(14);
            match self
                .driver
                .driver
                .collect(slot, 0, MAX_ACCESS_UNIT_BYTES as usize)
                .map_err(|error| self.shared.native_error(error))?
            {
                NativeCollect::Pending => break,
                NativeCollect::Ready(packet) => self.complete(0, packet)?,
            }
        }
        if wait_ms > 0
            && let Some(first) = self.inflight.first()
        {
            let slot = first.slot;
            if let NativeCollect::Ready(packet) = self
                .driver
                .driver
                .collect(slot, wait_ms, MAX_ACCESS_UNIT_BYTES as usize)
                .map_err(|error| self.shared.native_error(error))?
            {
                self.complete(0, packet)?;
            }
        }
        Ok(())
    }

    /// Turns one completed native submission into a delivered packet.
    ///
    /// The native output timestamp and duration must be exactly the source values that
    /// were submitted, and the delivery order must stay strictly increasing: an
    /// unexpected reorder is an explicit failure, never a silent sort or a duplicated
    /// frame.
    fn complete(
        &mut self,
        index: usize,
        packet: driver::NativePacket,
    ) -> Result<(), RecorderError> {
        let entry = self.inflight.remove(index);
        crate::cursor_preview_diagnostics::record("encoder_complete", None, || {
            serde_json::json!({"pts_ns": packet.pts_ns, "frame_index": packet.frame_index,
                "bytes": packet.data.len()})
        });
        self.shared.inflight.fetch_sub(1, Ordering::AcqRel);
        if self.inflight.is_empty() {
            self.oldest_submission = None;
        }
        self.release_reservation(entry.reserved);
        if packet.pts_ns != entry.pts || packet.duration_ns != entry.duration {
            self.shared.native_fault.store(true, Ordering::Release);
            EncoderCounters::bump(&self.shared.counters.timestamp_mismatches);
            EncoderCounters::bump(&self.shared.counters.packets_discarded_on_failure);
            self.release_lease(entry.lease);
            return Err(internal_error(format!(
                "The hardware encoder returned timestamp {} ns with duration {} ns for a frame submitted as {} ns with duration {} ns; the source timeline did not survive the native encode.",
                packet.pts_ns, packet.duration_ns, entry.pts, entry.duration
            )));
        }
        if let Some(previous) = self.last_emitted_pts
            && packet.pts_ns <= previous
        {
            self.shared.native_fault.store(true, Ordering::Release);
            EncoderCounters::bump(&self.shared.counters.timestamp_regressions);
            EncoderCounters::bump(&self.shared.counters.packets_discarded_on_failure);
            self.release_lease(entry.lease);
            return Err(internal_error(format!(
                "The hardware encoder delivered {} ns after {previous} ns, so its output order is not this session's submission order.",
                packet.pts_ns
            )));
        }
        self.last_emitted_pts = Some(packet.pts_ns);
        self.record_native_picture(packet.picture_type, packet.frame_index);
        let length = packet.data.len() as u64;
        let encoded = EncodedPacket {
            track_id: 0,
            // No reordering: DTS = PTS, both the verified native output timestamp.
            pts: MediaTimestamp(packet.pts_ns),
            dts: MediaTimestamp(packet.pts_ns),
            duration_ns: packet.duration_ns,
            keyframe: packet.keyframe,
            data: packet.data,
        };
        if let Err(error) = self.shared.push_packet(encoded) {
            EncoderCounters::bump(&self.shared.counters.packets_discarded_on_failure);
            self.release_lease(entry.lease);
            return Err(error);
        }
        // One delivered access unit per completed native picture, counted both ways so a
        // consumer never has to assume the equality: this is the only producer of either
        // counter, and `identities` reports the pair.
        EncoderCounters::bump(&self.shared.counters.frames_completed);
        EncoderCounters::bump(&self.shared.counters.packets_emitted);
        EncoderCounters::add(&self.shared.counters.bytes_emitted, length);
        self.release_lease(entry.lease);
        Ok(())
    }

    /// Offers queued work to the driver while a bounded output credit exists.
    fn submit_local(&mut self) -> Result<(), RecorderError> {
        while let Some(job) = self.local.pop_front() {
            if !self.credit_available() {
                // Consumer pressure, not a native stall: hold the accepted input and
                // wait for the consumer to free a bounded credit.
                self.shared.set_stage(4);
                self.local.push_front(job);
                return Ok(());
            }
            match self.try_submit(job)? {
                SubmitAttempt::Submitted => {
                    self.busy_since = None;
                }
                SubmitAttempt::Retry(job) => {
                    // The driver is busy and did not take the input. The input stays
                    // owned here until a retry succeeds.
                    self.local.push_front(job);
                    self.check_busy_watchdog()?;
                    return Ok(());
                }
            }
        }
        self.busy_since = None;
        Ok(())
    }

    /// True when one more native submission fits the packet and byte bounds. The full
    /// per-access-unit bound is reserved before any native work is submitted.
    /// True when one more native submission fits both bounds.
    ///
    /// The byte bound charges everything Frame owns at the same instant: the packets in the
    /// delivery queue, every in-flight reservation, the driver's reusable collection
    /// (scratch) buffer and the reservation being made now. The scratch buffer exists for
    /// the whole session, so it is charged permanently rather than only while copying.
    fn credit_available(&self) -> bool {
        held_credits_available(
            self.shared.packets_held.load(Ordering::Acquire) as u64,
            self.shared.bytes_held.load(Ordering::Acquire),
            self.inflight.len() as u64,
            self.reserved_bytes,
            u64::from(MAX_ACCESS_UNIT_BYTES),
        )
    }

    fn reserve(&mut self) {
        self.reserved_bytes = self
            .reserved_bytes
            .saturating_add(u64::from(MAX_ACCESS_UNIT_BYTES));
        debug_assert!(
            self.reserved_bytes <= MAX_PACKET_BYTES,
            "the reserved packet credits must stay inside the owned-byte budget"
        );
    }

    fn release_reservation(&mut self, reserved: u64) {
        self.reserved_bytes = self.reserved_bytes.saturating_sub(reserved);
    }

    /// Submits one accepted input, or returns it unchanged when the driver was busy.
    fn try_submit(&mut self, job: Job) -> Result<SubmitAttempt, RecorderError> {
        let Some(texture) = job.frame.surface.encode_texture() else {
            EncoderCounters::bump(&self.shared.counters.rejected_inputs);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
            return Err(internal_error(
                "A captured surface did not expose the GPU backing this encoder session was opened for."
                    .to_string(),
            ));
        };
        if texture.identity.pool != self.expected_pool
            || texture.identity.device != self.expected_device
        {
            EncoderCounters::bump(&self.shared.counters.rejected_inputs);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
            return Err(internal_error(format!(
                "A captured surface from pool {} on device 0x{:X} was submitted to an encoder session opened for pool {} on device 0x{:X}; cross-pool and cross-device input is rejected.",
                texture.identity.pool,
                texture.identity.device,
                self.expected_pool,
                self.expected_device
            )));
        }
        if texture.identity.width != self.settings.width
            || texture.identity.height != self.settings.height
        {
            EncoderCounters::bump(&self.shared.counters.rejected_inputs);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
            return Err(internal_error(format!(
                "A {}x{} surface was submitted to a {}x{} encoder session; this session rejects a dimension change instead of reconfiguring.",
                texture.identity.width,
                texture.identity.height,
                self.settings.width,
                self.settings.height
            )));
        }
        let backing = job.frame.surface.encode_backing();
        let registration = match self.registration_for(texture, backing) {
            Ok(Some(registration)) => registration,
            Ok(None) => {
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                self.release_lease(job.frame);
                return Ok(SubmitAttempt::Submitted);
            }
            Err(error) => {
                // The input never reached the driver: it is a rejected input, and its
                // lease is released here.
                EncoderCounters::bump(&self.shared.counters.rejected_inputs);
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                self.release_lease(job.frame);
                return Err(error);
            }
        };
        let pts = job.pts.0;
        let duration = job.duration.get();
        // The input is parked here while the driver holds it, so a driver panic still
        // leaves it visible to this worker's unwind accounting instead of vanishing
        // inside the call.
        self.shared.set_stage(7);
        self.handing_over = Some(job);
        self.shared.set_stage(11);
        if self.shared.replay_keyframes.load(Ordering::Acquire)
            && self
                .last_replay_idr_pts
                .is_none_or(|last| pts.saturating_sub(last) >= 1_000_000_000)
        {
            self.force_idr = true;
        }
        let forced_idr = self.force_idr;
        let diagnostic_start = crate::cursor_preview_diagnostics::enabled()
            .then(crate::cursor_preview_diagnostics::now_ns);
        crate::cursor_preview_diagnostics::record(
            "encoder_submit_begin",
            None,
            || serde_json::json!({"pts_ns": pts, "registration": registration}),
        );
        let outcome = self
            .driver
            .driver
            .submit(registration, pts, duration, self.force_idr)
            .map_err(|error| self.shared.native_error(error));
        crate::cursor_preview_diagnostics::record("encoder_submit_end", None, || {
            serde_json::json!({"pts_ns": pts, "registration": registration,
                "elapsed_ns": diagnostic_start.map(|start| crate::cursor_preview_diagnostics::now_ns().saturating_sub(start)),
                "outcome": format!("{outcome:?}")})
        });
        self.shared.set_stage(12);
        let job = self
            .handing_over
            .take()
            .expect("the handed-over input is taken back exactly once");
        match outcome {
            Ok(NativeSubmit::Pending { slot }) => {
                self.reserve();
                if self.inflight.is_empty() {
                    self.oldest_submission = Some(Instant::now());
                }
                self.inflight.push(InFlight {
                    slot,
                    lease: job.frame,
                    pts,
                    duration,
                    reserved: u64::from(MAX_ACCESS_UNIT_BYTES),
                });
                self.shared.inflight.fetch_add(1, Ordering::AcqRel);
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                EncoderCounters::bump(&self.shared.counters.native_submissions);
                self.force_idr = false;
                if forced_idr {
                    self.last_replay_idr_pts = Some(pts);
                }
                Ok(SubmitAttempt::Submitted)
            }
            Ok(NativeSubmit::Buffered { slot }) => {
                // Buffered acceptance: the driver holds the picture and this output slot
                // and mapping stay owned until its access unit arrives (with later input
                // or with EOS). The lease is retained like any other outstanding picture.
                self.reserve();
                if self.inflight.is_empty() {
                    self.oldest_submission = Some(Instant::now());
                }
                self.inflight.push(InFlight {
                    slot,
                    lease: job.frame,
                    pts,
                    duration,
                    reserved: u64::from(MAX_ACCESS_UNIT_BYTES),
                });
                self.shared.inflight.fetch_add(1, Ordering::AcqRel);
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                EncoderCounters::bump(&self.shared.counters.native_buffered);
                self.force_idr = false;
                if forced_idr {
                    self.last_replay_idr_pts = Some(pts);
                }
                Ok(SubmitAttempt::Submitted)
            }
            Ok(NativeSubmit::Busy) => {
                EncoderCounters::bump(&self.shared.counters.native_busy_retries);
                if self.busy_since.is_none() {
                    self.busy_since = Some(Instant::now());
                }
                Ok(SubmitAttempt::Retry(job))
            }
            Err(error) => {
                // A failed native submit may still own a mapping. Confirm cleanup on
                // this worker before returning the texture to the capture pool.
                self.shared.fail(error.clone());
                let cleanup = self.close_driver();
                let error = combine_cleanup_errors(Some(error), cleanup.err());
                EncoderCounters::bump(&self.shared.counters.native_failures);
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                self.release_lease(job.frame);
                Err(error)
            }
        }
    }

    /// Finds or creates the bounded native registration for one capture texture.
    ///
    /// `Ok(None)` means the texture was registered but the input was already discarded
    /// (used by the failure paths that must still account the input).
    fn registration_for(
        &mut self,
        texture: EncodeTexture,
        backing: SharedPayload,
    ) -> Result<Option<u32>, RecorderError> {
        if let Some(existing) = self
            .registrations
            .iter()
            .find(|entry| entry.texture == texture.identity.texture)
        {
            return Ok(Some(existing.index));
        }
        if self.registrations.len() >= MAX_REGISTRATIONS {
            return Err(internal_error(format!(
                "A fifth capture texture was submitted, but this session registers at most {MAX_REGISTRATIONS} textures from its own pool."
            )));
        }
        let index = self
            .driver
            .driver
            .register_input(texture)
            .map_err(|error| self.shared.native_error(error))?;
        self.registrations.push(Registration {
            texture: texture.identity.texture,
            index,
            // The backing is retained for as long as the native registration exists.
            _backing: backing,
        });
        EncoderCounters::bump(&self.shared.counters.registrations);
        Ok(Some(index))
    }

    /// Signals end of stream, retrying the documented busy result inside the drain
    /// watchdog.
    fn submit_eos(&mut self) -> Result<(), RecorderError> {
        match self
            .driver
            .driver
            .begin_eos()
            .map_err(|error| self.shared.native_error(error))?
        {
            NativeEos::Submitted => {
                self.eos_submitted = true;
                self.eos_started = Some(Instant::now());
                EncoderCounters::bump(&self.shared.counters.eos_submissions);
                Ok(())
            }
            NativeEos::Busy => {
                EncoderCounters::bump(&self.shared.counters.eos_busy_retries);
                if self.eos_started.is_none() {
                    self.eos_started = Some(Instant::now());
                }
                self.check_drain_watchdog()
            }
        }
    }

    fn poll_eos(&mut self) -> Result<bool, RecorderError> {
        if self
            .driver
            .driver
            .poll_eos(NATIVE_WAIT_MS)
            .map_err(|error| self.shared.native_error(error))?
        {
            // A manual-reset event stays signalled, so the acknowledgement is recorded once
            // even though the drain loop keeps polling until every output has arrived.
            if !self.shared.eos_acknowledged.swap(true, Ordering::AcqRel) {
                EncoderCounters::bump(&self.shared.counters.eos_acknowledgements);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Successful drain: every accepted input completed, EOS was acknowledged, and only
    /// then are the registrations and the native session released. The success outcome is
    /// published after that cleanup, so no consumer can observe a completed drain while a
    /// registration or the session is still being released.
    fn finish_drain(&mut self) -> Result<(), RecorderError> {
        self.finished = true;
        let registrations = self.release_registrations();
        let closed = self.close_driver();
        match (registrations, closed) {
            (Ok(()), Ok(())) => {
                self.shared
                    .publish(TerminalOutcome::Clean { drained: true });
                Ok(())
            }
            (primary, secondary) => {
                let error = combine_cleanup_errors(primary.err(), secondary.err());
                self.shared.publish(TerminalOutcome::Failed(error.clone()));
                self.shared.fail(error.clone());
                Err(error)
            }
        }
    }

    /// Early stop: every accepted input and every abandoned native submission is
    /// accounted for, the capture leases are released and the native session is closed.
    /// The session reports the incomplete finalization; the worker itself did not fail.
    fn shutdown_early(&mut self) -> Result<(), RecorderError> {
        self.finished = true;
        // The native session is destroyed (which unmaps inputs, releases registrations and
        // frees the bitstream buffers) *before* any in-flight capture lease is released, so
        // no slot can be recycled while the driver could still read it. The phase is
        // recorded first so the exact unfinished call survives the teardown.
        self.shared.record_native_phase(self.driver.driver.as_ref());
        let registrations = self.release_registrations();
        let closed = self.close_driver();
        let failure = if registrations.is_err() || closed.is_err() {
            Some(combine_cleanup_errors(registrations.err(), closed.err()))
        } else {
            None
        };
        while let Some(entry) = self.inflight.pop() {
            self.shared.inflight.fetch_sub(1, Ordering::AcqRel);
            EncoderCounters::bump(&self.shared.counters.slots_abandoned_at_stop);
            self.release_reservation(entry.reserved);
            self.release_lease(entry.lease);
        }
        if let Some(job) = self.handing_over.take() {
            EncoderCounters::bump(&self.shared.counters.inputs_discarded_on_stop);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
        }
        while let Some(job) = self.local.pop_front() {
            EncoderCounters::bump(&self.shared.counters.inputs_discarded_on_stop);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
        }
        while let Ok(job) = self.ingress.try_recv() {
            EncoderCounters::bump(&self.shared.counters.inputs_discarded_on_stop);
            self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
            self.release_lease(job.frame);
        }
        if let Some(error) = failure {
            self.shared.publish(TerminalOutcome::Failed(error.clone()));
            self.shared.fail(error.clone());
            return Err(error);
        }
        self.shared
            .publish(TerminalOutcome::Clean { drained: false });
        Ok(())
    }

    fn release_registrations(&mut self) -> Result<(), RecorderError> {
        let mut failure = None;
        while let Some(entry) = self.registrations.pop() {
            match self.driver.driver.unregister_input(entry.index) {
                Ok(()) => {
                    EncoderCounters::bump(&self.shared.counters.unregistrations);
                }
                Err(error) => {
                    // A failed unregister is not a release: the registration stays
                    // recorded, the counter does not move and the first cleanup failure is
                    // kept while the rest are still attempted.
                    self.registrations.push(entry);
                    if failure.is_none() {
                        failure = Some(error);
                    }
                    break;
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn close_driver(&mut self) -> Result<(), RecorderError> {
        let started = Instant::now();
        let result = self.driver.finish();
        EncoderCounters::add(
            &self.shared.counters.teardown_nanos,
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        result
    }

    /// Releases one retained capture lease exactly once, returning its slot.
    fn release_lease(&self, lease: CapturedFrame<WindowsSurface>) {
        crate::cursor_preview_diagnostics::record(
            "encoder_release",
            None,
            || serde_json::json!({"pts_ns": lease.timestamp.0}),
        );
        drop(lease);
        let previous = self.shared.retained.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "a lease is released at most once");
        self.shared.notify_owner();
    }

    fn check_progress_watchdog(&mut self) -> Result<(), RecorderError> {
        if let Some(started) = self.oldest_submission
            && started.elapsed() > PROGRESS_WATCHDOG
        {
            self.shared.native_fault.store(true, Ordering::Release);
            return Err(internal_error(format!(
                "The hardware encoder did not complete a submitted picture within {} ms.",
                PROGRESS_WATCHDOG.as_millis()
            )));
        }
        Ok(())
    }

    fn check_busy_watchdog(&mut self) -> Result<(), RecorderError> {
        if let Some(since) = self.busy_since
            && since.elapsed() > PROGRESS_WATCHDOG
        {
            self.shared.native_fault.store(true, Ordering::Release);
            return Err(internal_error(format!(
                "The hardware encoder reported itself busy for {} ms without accepting a submitted picture.",
                PROGRESS_WATCHDOG.as_millis()
            )));
        }
        Ok(())
    }

    fn check_drain_watchdog(&mut self) -> Result<(), RecorderError> {
        if let Some(started) = self.eos_started
            && started.elapsed() > DRAIN_WATCHDOG
        {
            self.shared.native_fault.store(true, Ordering::Release);
            // The drain watchdog covers only native progress: a consumer that has not
            // drained its queue yet is bounded by the packet credits, not by this timer.
            return Err(internal_error(format!(
                "The hardware encoder did not acknowledge end of stream within {} ms.",
                DRAIN_WATCHDOG.as_millis()
            )));
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Safety net: if the worker left this scope without completing its accounting —
        // an unwind that the thread boundary caught, or any path that forgot to account —
        // every lease and submission it still owns is released and counted exactly once.
        self.abandon_after_unwind();
    }
}

// -------------------------------------------------------------------- facade

/// One running hardware H.264 encoder session (FRAME-0104).
///
/// It owns one worker thread, its native session and the bounded packet queue. A capture
/// lease handed to [`PacketEncoder::submit`] stays owned until its picture has completed
/// natively and its input has been unmapped; it is not recycled early. NVENC consumes
/// GPU textures directly. Isolated MF copies through bounded CPU staging and IPC;
/// its parent uses the capture device's multithread-protected immediate context.
pub struct WindowsH264Encoder {
    shared: Arc<EncoderShared>,
    control: Arc<EncodeControl>,
    worker: Option<JoinGuard>,
    ingress: SyncSender<Job>,
    config: NativeEncoderConfig,
    settings: NativeEncoderSettings,
    info: EncodedStreamInfo,
    stopped: bool,
    stop_result: Option<RecorderError>,
    /// The device and pool this session was opened for; any other lease is rejected.
    identity: SurfaceIdentity,
    surface_format: SurfaceFormat,
    surface_signal: Option<crate::surface::SurfaceSignal>,
    last_accepted_pts: Option<u64>,
}

impl std::fmt::Debug for WindowsH264Encoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsH264Encoder")
            .field("settings", &self.settings.describe())
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl WindowsH264Encoder {
    /// Bound random-access spacing in media time even on a static source.
    pub fn set_replay_keyframes(&mut self, enabled: bool) {
        self.shared
            .replay_keyframes
            .store(enabled, Ordering::Release);
    }
    pub fn set_wake_signal(&mut self, wake: frame_capture::PipelineWake) {
        let _ = self.shared.owner_wake.set(wake);
        self.shared.notify_owner();
    }
    /// The published worker failure, without consuming a queued packet.
    pub fn failure(&self) -> Option<RecorderError> {
        self.shared.terminal.get().cloned()
    }

    /// A terminal failure whose cause was a native call or progress watchdog.
    /// Policy/queue errors and unwinds cannot trigger alternative backend reuse.
    pub fn native_failure(&self) -> Option<RecorderError> {
        self.shared
            .native_fault
            .load(Ordering::Acquire)
            .then(|| self.failure())
            .flatten()
    }
    /// Opens only when the first actual capture texture matches the policy's
    /// selected encoder and adapter. Topology changes fail before mux creation.
    pub fn open_selected(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
        expected_id: &str,
    ) -> Result<Self, RecorderError> {
        let encoder = if expected_id.starts_with("nvidia-mf-h264-d3d11:") {
            Self::open_isolated_mf(config, lease)?
        } else if [
            "nvidia-mf-av1-d3d11:",
            "intel-mf-av1-d3d11:",
            "amd-mf-av1-d3d11:",
        ]
        .iter()
        .any(|prefix| expected_id.starts_with(prefix))
        {
            Self::open_mf_av1(config, lease)?
        } else if expected_id.starts_with("nvidia-nvenc-av1-d3d11:")
            || expected_id.starts_with("nvidia-nvenc-av1-main10-d3d11:")
        {
            Self::open_av1(config, lease)?
        } else if expected_id.starts_with("nvidia-nvenc-hevc-main10-d3d11:") {
            Self::open_hevc_main10(config, lease)?
        } else {
            Self::open(config, lease)?
        };
        if encoder.info.capability.id != expected_id {
            return Err(RecorderError {
                code: ErrorCode::EncoderUnavailable,
                message: format!(
                    "Selected encoder {expected_id} does not match the first captured surface's {}.",
                    encoder.info.capability.id
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Refresh the display and encoder selection.".into(),
            });
        }
        Ok(encoder)
    }
    /// Opens a hardware session for `config` on the Direct3D device that backs `lease`.
    ///
    /// NVENC registration is device-bound, so the accepted design encodes on the capture
    /// path's own device instead of creating a second one. The lease is borrowed for the
    /// preparation only; a lease from another pool, device or geometry is rejected
    /// rather than silently re-initialized.
    pub fn open(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        if lease.format() != SurfaceFormat::Bgra8 {
            return Err(preparation_error(
                "The H.264 encoder accepts BGRA8 SDR capture surfaces only; an FP16 HDR surface requires an explicit 10-bit conversion and HEVC session.".to_string(),
            ));
        }
        let texture = lease.encode_texture().ok_or_else(|| {
            preparation_error(
                "The capture lease does not expose the GPU backing the hardware encoder needs."
                    .to_string(),
            )
        })?;
        if texture.identity.width != config.width || texture.identity.height != config.height {
            return Err(invalid_request_error(format!(
                "The encoder was requested for {}x{} but the capture lease is {}x{}; a dimension change needs a fresh session.",
                config.width, config.height, texture.identity.width, texture.identity.height
            )));
        }
        if texture.device.is_null() || texture.texture.is_null() {
            return Err(preparation_error(
                "The capture lease did not expose a usable Direct3D device and texture."
                    .to_string(),
            ));
        }
        let adapter = native::device_adapter(texture.device)?;
        if !matches!(adapter.vendor_id, 0x10DE | 0x8086 | 0x1002) {
            return Err(RecorderError {
                code: ErrorCode::EncoderUnavailable,
                message: format!(
                    "Capture texture is on {} (DXGI LUID {:016X}, vendor {:04X}); no supported H.264 hardware path exists for that vendor. No cross-adapter copy or CPU fallback is configured.",
                    adapter.description, adapter.luid, adapter.vendor_id,
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Select an encoder qualified for this capture adapter.".into(),
            });
        }
        // The session is opened on its owner worker, not on the caller: the worker is the
        // thread that will use it, and a preparation that fails there is reported through
        // the same readiness handshake.
        let backing = lease.encode_backing();
        // The device pointer is carried as its address: the worker rebuilds the pointer
        // and opens the session on the thread that will own it. The lease-backed payload
        // keeps the device alive for the whole handshake.
        let device_address = texture.device as usize;
        let mf_adapter = adapter.clone();
        let mut encoder = Self::start_with_prepared(
            config,
            texture.identity,
            backing,
            lease.is_preparation(),
            move || {
                let device = device_address as *mut std::ffi::c_void;
                if mf_adapter.vendor_id == 0x10DE {
                    native::NativeDriver::open(device, &config)
                        .map(|driver| Box::new(driver) as Box<dyn NvencDriver>)
                } else {
                    mf_process::MfProcessDriver::open(device, &mf_adapter, &config)
                        .map(|driver| Box::new(driver) as Box<dyn NvencDriver>)
                }
            },
        )?;
        encoder.info.capability.id = match adapter.vendor_id {
            0x10DE => format!("nvidia-nvenc-h264-d3d11:luid={:016X}", adapter.luid),
            0x8086 => format!("intel-mf-h264-d3d11:luid={:016X}", adapter.luid),
            0x1002 => format!("amd-mf-h264-d3d11:luid={:016X}", adapter.luid),
            _ => unreachable!("vendor checked above"),
        };
        if adapter.vendor_id == 0x10DE {
            encoder.info.colour = Some(VideoColour::BT709_LIMITED);
        }
        Ok(encoder)
    }

    /// Explicit HEVC Main10, accepting source-signalled P010 or linear scRGB.
    pub fn open_hevc_main10(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
    ) -> Result<Self, RecorderError> {
        Self::open_native_profile(config, lease, NativeVideoProfile::HevcMain10)
    }

    /// AV1 Main 8/10-bit; source format determines depth, never an HDR display label.
    pub fn open_av1(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
    ) -> Result<Self, RecorderError> {
        let profile = if matches!(
            lease.format(),
            SurfaceFormat::P010 | SurfaceFormat::ScRgbFloat16
        ) {
            NativeVideoProfile::Av1Main10
        } else {
            NativeVideoProfile::Av1Main8
        };
        Self::open_native_profile(config, lease, profile)
    }

    pub fn open_mf_av1(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        if lease.format() != SurfaceFormat::Bgra8 {
            return Err(preparation_error(
                "MF AV1 currently accepts BGRA SDR capture through its negotiated NV12 transport."
                    .into(),
            ));
        }
        native_input_signal(NativeVideoProfile::Av1Main8, lease.format(), lease.signal())?;
        let texture = lease
            .encode_texture()
            .ok_or_else(|| preparation_error("MF AV1 capture texture is unavailable.".into()))?;
        if texture.identity.width != config.width || texture.identity.height != config.height {
            return Err(preparation_error(
                "MF AV1 capture geometry differs from the requested session.".into(),
            ));
        }
        let adapter = native::device_adapter(texture.device)?;
        let id = mf_av1_id(&adapter)?;
        let device = texture.device as usize;
        let mut encoder = Self::start_with_prepared(
            config,
            texture.identity,
            lease.encode_backing(),
            lease.is_preparation(),
            move || {
                mf_process::MfProcessDriver::open_profile(
                    device as *mut std::ffi::c_void,
                    &adapter,
                    &config,
                    2,
                )
                .map(|driver| Box::new(driver) as Box<dyn NvencDriver>)
            },
        )?;
        encoder.info.capability = EncoderCapability {
            id,
            codec: "av1".into(),
            hardware: true,
        };
        encoder.info.bitstream_format = BitstreamFormat::Av1Obu;
        encoder.info.colour = Some(VideoColour::BT709_LIMITED);
        Ok(encoder)
    }

    /// Native profile entry point for GPU NV12/P010 sources. These are registered
    /// directly on their capture device; RGB conversion is used only for scRGB.
    pub fn open_native_profile(
        config: NativeEncoderConfig,
        lease: &WindowsSurface,
        profile: NativeVideoProfile,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        let format = lease.format();
        let signal = lease.signal();
        let (input_format, colour, convert) = native_input_signal(profile, format, signal)?;
        if profile.codec() == "av1"
            && colour
                .mastering
                .is_some_and(|m| m.luminance_min > 262143.9999)
        {
            return Err(preparation_error(
                "AV1 mastering minimum luminance exceeds its metadata field.".into(),
            ));
        }
        let texture = lease
            .encode_texture()
            .ok_or_else(|| preparation_error("Native capture lease has no GPU texture.".into()))?;
        if texture.identity.width != config.width
            || texture.identity.height != config.height
            || texture.device.is_null()
            || texture.texture.is_null()
        {
            return Err(invalid_request_error(
                "Native capture geometry/device does not match the encoder request.".into(),
            ));
        }
        let adapter = native::device_adapter(texture.device)?;
        if adapter.vendor_id != 0x10DE {
            return Err(preparation_error(
                "This native profile requires a qualified NVENC encoder on the capture adapter."
                    .into(),
            ));
        }
        let device = texture.device as usize;
        let mut encoder = Self::start_with_prepared(
            config,
            texture.identity,
            lease.encode_backing(),
            lease.is_preparation(),
            move || {
                native::NativeDriver::open_signal(
                    device as *mut std::ffi::c_void,
                    &config,
                    profile.bridge_value(),
                    input_format,
                    Some(colour),
                    convert,
                )
                .map(|driver| Box::new(driver) as Box<dyn NvencDriver>)
            },
        )?;
        encoder.info.capability = EncoderCapability {
            id: profile.id(adapter.luid),
            codec: profile.codec().into(),
            hardware: true,
        };
        encoder.info.bitstream_format = profile.bitstream();
        encoder.info.colour = Some(colour);
        encoder.surface_format = format;
        encoder.surface_signal = signal;
        Ok(encoder)
    }

    /// Assembles the facade around a driver that is prepared **on its own worker**.
    ///
    /// The factory runs on the worker thread, which is the thread that will use the
    /// session, so initialization, idle ownership and destruction all happen on the owner
    /// thread. The facade waits for a bounded readiness handshake and builds the stream
    /// metadata from the settings the driver actually reported. Production reaches this
    /// constructor through [`WindowsH264Encoder::open`]; the deterministic tests pass a
    /// factory that returns a scripted driver.
    #[cfg(test)]
    pub(crate) fn start_with<P>(
        config: NativeEncoderConfig,
        identity: SurfaceIdentity,
        backing: SharedPayload,
        prepare: P,
    ) -> Result<Self, RecorderError>
    where
        P: FnOnce() -> Result<Box<dyn NvencDriver>, RecorderError> + Send + 'static,
    {
        Self::start_with_prepared(config, identity, backing, false, prepare)
    }

    fn start_with_prepared<P>(
        config: NativeEncoderConfig,
        identity: SurfaceIdentity,
        backing: SharedPayload,
        prepare_pool: bool,
        prepare: P,
    ) -> Result<Self, RecorderError>
    where
        P: FnOnce() -> Result<Box<dyn NvencDriver>, RecorderError> + Send + 'static,
    {
        config.validate()?;
        let shared = Arc::new(EncoderShared::new());
        let control = Arc::new(EncodeControl::default());
        let (sender, receiver) = sync_channel(INGRESS_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel::<Result<NativeEncoderSettings, RecorderError>>(1);
        let worker_shared = Arc::clone(&shared);
        let worker_control = Arc::clone(&control);
        let expected = identity;
        let worker_backing = Arc::clone(&backing);
        let handle = std::thread::Builder::new()
            .name("frame-h264-encoder".to_string())
            .spawn(move || {
                let prepared = match prepare() {
                    Ok(driver) => {
                        // The successful session goes behind a cleanup guard before any
                        // fallible validation, so a refused configuration still destroys it
                        // on this owner thread.
                        let mut guard = OpenGuard::new(driver);
                        let settings = guard.driver().settings().clone();
                        if let Err(error) = settings.matches_request(&config) {
                            let _ = ready_tx.send(Err(error.clone()));
                            worker_shared.fail(error);
                            worker_shared.mark_worker_finished();
                            return;
                        }
                        if settings.codec_config.len() > MAX_CODEC_CONFIG_BYTES {
                            let error = preparation_error(format!(
                                "The native encoder reported {} initialization bytes, above the accepted {MAX_CODEC_CONFIG_BYTES}-byte limit.",
                                settings.codec_config.len()
                            ));
                            let _ = ready_tx.send(Err(error.clone()));
                            worker_shared.fail(error);
                            worker_shared.mark_worker_finished();
                            return;
                        }
                        if settings.codec_config.is_empty() {
                            let error = preparation_error(
                                "The native encoder reported no initialization bytes, so no decoder could start on this stream."
                                    .to_string(),
                            );
                            let _ = ready_tx.send(Err(error.clone()));
                            worker_shared.fail(error);
                            worker_shared.mark_worker_finished();
                            return;
                        }
                        if settings.width == 0 || settings.height == 0 {
                            let error = preparation_error(
                                "The native encoder reported a zero dimension.".to_string(),
                            );
                            let _ = ready_tx.send(Err(error.clone()));
                            worker_shared.fail(error);
                            worker_shared.mark_worker_finished();
                            return;
                        }
                        // The device and the registered textures stay alive through
                        // initialization, idle periods and cleanup, including zero frames.
                        Some(Worker {
                            shared: Arc::clone(&worker_shared),
                            control: worker_control,
                            driver: DriverGuard::new(guard.take(), Arc::clone(&worker_shared)),
                            ingress: receiver,
                            expected_device: expected.device,
                            expected_pool: expected.pool,
                            settings: settings.clone(),
                            _backing: worker_backing,
                            inflight: Vec::new(),
                            registrations: Vec::new(),
                            local: VecDeque::new(),
                            handing_over: None,
                            force_idr: true,
                            last_replay_idr_pts: None,
                            last_emitted_pts: None,
                            reserved_bytes: 0,
                            eos_submitted: false,
                            eos_started: None,
                            channel_empty: false,
                            busy_since: None,
                            oldest_submission: None,
                            finished: false,
                            iterations: 0,
                        })
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.clone()));
                        worker_shared.fail(error);
                        worker_shared.mark_worker_finished();
                        return;
                    }
                };
                let Some(mut worker) = prepared else {
                    return;
                };
                if prepare_pool {
                    let prepared = (|| {
                        for slot in 0..MAX_REGISTRATIONS {
                            let texture = worker._backing.encode_texture(slot).ok_or_else(||
                                preparation_error("Prepared capture pool has no encoder texture.".into()))?;
                            if texture.identity.pool != expected.pool
                                || texture.identity.device != expected.device
                                || texture.identity.width != config.width
                                || texture.identity.height != config.height
                            {
                                return Err(preparation_error("Prepared capture pool identity changed.".into()));
                            }
                            worker.registration_for(texture, Arc::clone(&worker._backing))?;
                        }
                        Ok::<_, RecorderError>(())
                    })();
                    if let Err(error) = prepared {
                        let _ = ready_tx.send(Err(error.clone()));
                        worker.shared.fail(error);
                        let _ = worker.shutdown_early();
                        worker.shared.mark_worker_finished();
                        return;
                    }
                }
                crate::cursor_preview_diagnostics::record("encoder_ready", None, ||
                    serde_json::json!({"prepared_pool": prepare_pool, "registrations": worker.registrations.len()}));
                if ready_tx.send(Ok(worker.settings.clone())).is_err() {
                    // The control thread went away before readiness: clean up on the owner
                    // thread and stop.
                    let _ = worker.shutdown_early();
                    worker_shared.mark_worker_finished();
                    return;
                }
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run())) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => worker.shared.fail(error),
                    Err(_) => {
                        // The panic was just caught, so `thread::panicking()` is already
                        // false: the unwind accounting is explicit here.
                        worker.abandon_after_unwind();
                        worker.shared.fail(internal_error(
                            "The hardware encoder worker unwound, so its native session was released without completing the stream."
                                .to_string(),
                        ));
                    }
                }
                worker.shared.mark_worker_finished();
            })
            .map_err(|error| {
                preparation_error(format!(
                    "The hardware encoder worker thread could not be created: {error}"
                ))
            })?;
        // Ownership starts here: every path below either hands the guard to the facade or
        // joins the worker after signalling stop.
        let mut guard = JoinGuard::new(handle, Arc::clone(&control));

        let started = Instant::now();
        let settings = match ready_rx.recv_timeout(PREPARATION_WATCHDOG) {
            Ok(Ok(settings)) => settings,
            Ok(Err(error)) => {
                guard.stop_and_join();
                return Err(error);
            }
            Err(_) => {
                guard.stop_and_join();
                return Err(preparation_error(format!(
                    "Hardware encoder preparation did not finish within {} ms; the worker was signalled to stop and joined, so no thread is left behind. A native preparation call that cannot be interrupted is not cancellable.",
                    PREPARATION_WATCHDOG.as_millis()
                )));
            }
        };
        let _ = started;
        let info = EncodedStreamInfo {
            capability: EncoderCapability {
                id: format!(
                    "nvidia-nvenc-h264-d3d11:{}x{}",
                    settings.width, settings.height
                ),
                codec: "h264".to_string(),
                hardware: true,
            },
            width: NonZeroU32::new(settings.width).ok_or_else(|| {
                preparation_error("The native encoder reported a zero width.".to_string())
            })?,
            height: NonZeroU32::new(settings.height).ok_or_else(|| {
                preparation_error("The native encoder reported a zero height.".to_string())
            })?,
            nominal_rate_num: NonZeroU32::new(settings.rate_num).unwrap_or(config.rate_num),
            nominal_rate_den: NonZeroU32::new(settings.rate_den).unwrap_or(config.rate_den),
            bitstream_format: BitstreamFormat::H264AnnexB,
            colour: None,
            codec_config: settings.codec_config.clone(),
            allows_reordering: false,
        };
        Ok(Self {
            shared,
            control,
            worker: Some(guard),
            ingress: sender,
            config,
            settings,
            info,
            stopped: false,
            stop_result: None,
            identity,
            surface_format: SurfaceFormat::Bgra8,
            surface_signal: None,
            last_accepted_pts: None,
        })
    }

    /// Test seam: prepares an already-built driver through the same worker handshake.
    #[cfg(test)]
    pub(crate) fn with_driver(
        config: NativeEncoderConfig,
        identity: SurfaceIdentity,
        driver: Box<dyn NvencDriver>,
        backing: SharedPayload,
    ) -> Result<Self, RecorderError> {
        Self::start_with(config, identity, backing, move || Ok(driver))
    }
    /// The effective native configuration, as reported by the driver.
    pub fn qp_statistics(&self) -> Option<NativeQpStatistics> {
        self.shared
            .native_stats
            .lock()
            .ok()
            .and_then(|stats| stats.qp)
    }

    pub fn settings(&self) -> &NativeEncoderSettings {
        &self.settings
    }

    /// The requested configuration.
    pub fn config(&self) -> NativeEncoderConfig {
        self.config
    }

    /// The immutable stream metadata advertised before the first frame.
    pub fn info(&self) -> &EncodedStreamInfo {
        &self.info
    }

    /// One snapshot of the bounded delivery counters, with the queue gauges included.
    pub fn counters(&self) -> EncoderCounterSnapshot {
        let mut snapshot = self.shared.counters.snapshot();
        snapshot.packets_held = self.shared.packets_held.load(Ordering::Acquire) as u64;
        snapshot.bytes_held = self.shared.bytes_held.load(Ordering::Acquire);
        snapshot.retained_leases = self.shared.retained.load(Ordering::Acquire) as u64;
        snapshot.queued_inputs = self.shared.queued_inputs.load(Ordering::Acquire) as u64;
        snapshot.inflight = self.shared.inflight.load(Ordering::Acquire) as u64;
        snapshot
    }

    /// Typed read-only performance counters for this running (or stopped) session.
    ///
    /// This is the small data view of [`Self::counters`]: the frame and packet counts, the
    /// bounded ingress and delivery gauges and the disjoint discard categories, with the
    /// capacities the counters are bounded by. It never parses [`Self::diagnostics`] and
    /// never waits for the worker. A caller that wants a rate samples it on its own clock
    /// (for example through `frame_metrics::PipelineMetrics`), and measures
    /// [`EncoderSessionStats::completed_frames`], only while
    /// [`EncoderSessionStats::packets_match_frames`] holds.
    pub fn stats(&self) -> EncoderSessionStats {
        let counters = self.counters();
        EncoderSessionStats {
            accepted_inputs: counters.accepted_inputs,
            backpressure_returns: counters.backpressure_returns,
            rejected_inputs: counters.rejected_inputs,
            completed_frames: counters.frames_completed,
            packets_emitted: counters.packets_emitted,
            packets_dequeued: counters.packets_dequeued,
            packets_held: counters.packets_held,
            packet_capacity: MAX_PACKETS as u64,
            queued_inputs: counters.queued_inputs,
            ingress_capacity: INGRESS_CAPACITY as u64,
            queue_high_water: counters.queue_high_water,
            submitted_in_flight: counters.submitted_in_flight(),
            retained_leases: counters.retained_leases,
            lease_capacity: MAX_ENCODER_LEASES as u64,
            native_failures: counters.native_failures,
            discarded_on_stop: counters.inputs_discarded_on_stop,
            discarded_on_failure: counters.inputs_discarded_on_failure,
            discarded_on_unwind: counters.inputs_discarded_on_unwind,
        }
    }

    /// Final MF transfer diagnostics, available after stop when explicitly enabled.
    /// None means disabled or a different encoder; it never means zero transfer cost.
    pub fn mf_transfer_stats(&self) -> Option<MfTransferStats> {
        self.shared
            .mf_transfer_stats
            .try_lock()
            .ok()
            .and_then(|stats| *stats)
    }

    /// True when end of stream was acknowledged, every accepted input is accounted for
    /// and the delivery queue is empty.
    pub fn drained(&self) -> bool {
        self.shared.drain_reached()
    }

    /// The last native call phase this session observed.
    ///
    /// It is recorded before native cleanup whenever the worker abandons work, so a
    /// native call that never returned is named from the bridge's own record instead of
    /// being inferred from unchanged counters.
    /// Copies bounded native session records without accessing a live encoder object.
    pub fn native_call_witness() -> String {
        native::global_phase()
    }

    pub fn last_native_phase(&self) -> String {
        self.shared.native_phase()
    }

    /// The process-global native call phase: the last call any session began, with its
    /// thread and running flag. A thread inside a call that never returns cannot publish
    /// anything itself, so this is the evidence a witness reads.
    pub fn native_call_phase(&self) -> String {
        native::global_phase()
    }

    /// The encoder's live delivery and native-call state, for a watchdog trace.
    pub fn live_trace(&self) -> String {
        let counters = self.counters();
        format!(
            "stage={} stage_visits=[{}] phase_stats={} global_phase={} phase={} in_flight={} retained={} queued={} emitted={} dequeued={} accepted={} held={} bytes_held={} terminal={}",
            self.shared.stage(),
            self.shared.stage_counts(),
            self.shared.native_stats_summary(),
            native::global_phase(),
            self.shared.native_phase(),
            counters.submitted_in_flight(),
            counters.retained_leases,
            counters.queued_inputs,
            counters.packets_emitted,
            counters.packets_dequeued,
            counters.accepted_inputs,
            counters.packets_held,
            counters.bytes_held,
            self.shared.terminal.get().is_some()
        )
    }

    /// Bounded, human-readable encoder diagnostics: the effective configuration, the
    /// delivery counters, the native call counts and the accounting identities as they
    /// were checked.
    pub fn diagnostics(&self) -> String {
        let counters = self.counters();
        let native = self
            .shared
            .native_stats
            .lock()
            .map(|stats| *stats)
            .unwrap_or_default();
        let mut text = String::new();
        text.push_str("Frame Windows hardware H.264 encoder (FRAME-0104)\n");
        text.push_str(
            "This report describes one bounded encoder session: a raw Annex-B elementary stream with no container, no mux, no audio and no UI binding.\n",
        );
        text.push_str(&format!(
            "state: {} (worker_finished={}, eos_acknowledged={}, drain_requested={})\n",
            if self.stopped { "stopped" } else { "running" },
            self.shared.worker_finished.load(Ordering::Acquire),
            self.shared.eos_acknowledged.load(Ordering::Acquire),
            self.control.drain.load(Ordering::Acquire)
        ));
        text.push_str(&format!(
            "accepted initialization snapshot: {}\n",
            self.settings.describe()
        ));
        text.push_str(&format!(
            "native Advanced snapshot: {:?}; actual frame QP: {:?}\n",
            self.settings.advanced, native.qp
        ));
        text.push_str(&format!(
            "identity: device=0x{:X} pool={} session_texture=0x{:X}\n",
            self.identity.device, self.identity.pool, self.identity.texture
        ));
        text.push_str(&format!(
            "retained_leases: now={} bound={} high_water={}\n",
            counters.retained_leases, MAX_ENCODER_LEASES, counters.lease_high_water
        ));
        text.push_str(&format!(
            "packets: held={} held_bytes={} bound_packets={} bound_bytes={} high_water={}\n",
            counters.packets_held,
            counters.bytes_held,
            MAX_PACKETS,
            MAX_PACKET_BYTES,
            counters.queue_high_water
        ));
        text.push_str(&format!(
            "native: calls={} busy_results={} pending_inputs_at_last_native_report={}\n",
            native.native_calls, native.busy_results, native.pending_inputs
        ));
        text.push_str(&format!(
            "last_sampled_phase: {}\n",
            self.shared.native_phase()
        ));
        text.push_str(&format!(
            "native_call_phase: {}\n",
            self.native_call_phase()
        ));
        if let Some(retained) = self.shared.retained_summary() {
            text.push_str(&retained);
            text.push('\n');
        }
        match self
            .shared
            .native_last_picture
            .lock()
            .map(|evidence| *evidence)
            .unwrap_or(None)
        {
            Some((picture_type, frame_index)) => text.push_str(&format!(
                "native_last_picture: NV_ENC_PIC_TYPE={picture_type} frame_index={frame_index}\n"
            )),
            None => text.push_str("native_last_picture: none delivered\n"),
        }
        text.push_str("counters:\n");
        for (name, value) in counters.lines() {
            text.push_str(&format!("  {name}={value}\n"));
        }
        match counters.identities() {
            Ok(()) => text.push_str("accounting: every checked identity holds\n"),
            Err(detail) => text.push_str(&format!("accounting: NOT reconciled: {detail}\n")),
        }
        match self.shared.terminal.get() {
            Some(error) => text.push_str(&format!(
                "terminal_error: {} ({:?})\n",
                error.message, error.code
            )),
            None => text.push_str("terminal_error: none\n"),
        }
        text
    }

    fn terminal_error(&self) -> Result<(), RecorderError> {
        match self.shared.terminal.get() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn retained(&self) -> usize {
        self.shared.retained.load(Ordering::Acquire)
    }
}

impl PacketEncoder<EncoderInput<CapturedFrame<WindowsSurface>>> for WindowsH264Encoder {
    fn submit(
        &mut self,
        input: EncoderInput<CapturedFrame<WindowsSurface>>,
    ) -> Result<SubmitOutcome<EncoderInput<CapturedFrame<WindowsSurface>>>, RecorderError> {
        let EncoderInput { input, duration_ns } = input;
        self.terminal_error()?;
        if input.surface.is_preparation() {
            return Err(invalid_request_error(
                "An unfilled preparation surface is not a captured frame.".into(),
            ));
        }
        if self.control.drain.load(Ordering::Acquire) {
            return Err(invalid_request_error(
                "Admission is closed: this encoder has already been asked to drain.".to_string(),
            ));
        }
        // Timestamp policy: strictly increasing, positive duration and checked
        // timestamp+duration arithmetic. A repeated or backwards observation is an
        // error, never a manufactured duplicate frame.
        let pts = input.timestamp.0;
        if pts.checked_add(duration_ns.get()).is_none() {
            return Err(invalid_request_error(format!(
                "The frame at {pts} ns with a {} ns duration overflows the u64 nanosecond timeline.",
                duration_ns.get()
            )));
        }
        if let Some(previous) = self.last_accepted_pts
            && pts <= previous
        {
            return Err(invalid_request_error(format!(
                "The encoder accepts strictly increasing source timestamps: {pts} ns follows {previous} ns."
            )));
        }
        if input.width != self.config.width || input.height != self.config.height {
            return Err(invalid_request_error(format!(
                "A {}x{} frame cannot be submitted to a {}x{} encoder session.",
                input.width, input.height, self.config.width, self.config.height
            )));
        }
        if input.surface.format() != self.surface_format
            || input.surface.signal() != self.surface_signal
        {
            return Err(invalid_request_error(
                "The frame format or colour signal changed during the encoder session.".into(),
            ));
        }
        // Completed packet credits must block admission too. Otherwise three queued
        // leases plus caller lookahead exhaust the capture pool before refusal is seen.
        if self.shared.packets_held.load(Ordering::Acquire) >= MAX_PACKETS {
            EncoderCounters::bump(&self.shared.counters.backpressure_returns);
            return Ok(SubmitOutcome::Backpressure(EncoderInput {
                input,
                duration_ns,
            }));
        }
        // Bounded admission: this encoder never retains more than its share of the four
        // capture slots, and pressure returns the exact owned input.
        if self.shared.retained.fetch_add(1, Ordering::AcqRel) >= MAX_ENCODER_LEASES {
            self.shared.retained.fetch_sub(1, Ordering::AcqRel);
            EncoderCounters::bump(&self.shared.counters.backpressure_returns);
            return Ok(SubmitOutcome::Backpressure(EncoderInput {
                input,
                duration_ns,
            }));
        }
        EncoderCounters::raise(
            &self.shared.counters.lease_high_water,
            self.retained() as u64,
        );
        let job = Job {
            frame: input,
            pts: MediaTimestamp(pts),
            duration: duration_ns,
        };
        self.shared.queued_inputs.fetch_add(1, Ordering::AcqRel);
        match self.ingress.try_send(job) {
            Ok(()) => {
                self.last_accepted_pts = Some(pts);
                EncoderCounters::bump(&self.shared.counters.accepted_inputs);
                self.shared.notify_worker();
                Ok(SubmitOutcome::Accepted)
            }
            Err(TrySendError::Full(job)) => {
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                self.shared.retained.fetch_sub(1, Ordering::AcqRel);
                EncoderCounters::bump(&self.shared.counters.backpressure_returns);
                Ok(SubmitOutcome::Backpressure(EncoderInput {
                    input: job.frame,
                    duration_ns: job.duration,
                }))
            }
            Err(TrySendError::Disconnected(job)) => {
                self.shared.queued_inputs.fetch_sub(1, Ordering::AcqRel);
                self.shared.retained.fetch_sub(1, Ordering::AcqRel);
                // Nothing was accepted, so the caller keeps its frame. The worker is
                // gone, which is a terminal condition for this session.
                let error = self.terminal_error().err().unwrap_or_else(|| {
                    internal_error(
                        "The hardware encoder worker is gone, so no further frame can be accepted."
                            .to_string(),
                    )
                });
                self.shared.fail(error.clone());
                drop(job);
                Err(error)
            }
        }
    }

    fn try_packet(&mut self) -> Result<EncoderPoll, RecorderError> {
        // The published outcome is read first and consistently: a successful drain is only
        // visible after every cleanup step, so `Drained` can never precede a pending or
        // failed unregister or destroy.
        match self.shared.terminal_outcome() {
            Some(TerminalOutcome::Failed(error)) => return Err(error.clone()),
            Some(TerminalOutcome::Clean { .. }) | None => {}
        }
        self.terminal_error()?;
        if let Some(error) = &self.stop_result {
            return Err(error.clone());
        }
        if !self.stopped
            && self.shared.worker_finished.load(Ordering::Acquire)
            && !self.control.drain.load(Ordering::Acquire)
        {
            return Err(internal_error(
                "The hardware encoder worker stopped without a drain request, so no further packet can be delivered."
                    .to_string(),
            ));
        }
        let popped = match self.shared.packets.try_lock() {
            Ok(mut queue) => queue.pop(),
            Err(TryLockError::WouldBlock) => {
                EncoderCounters::bump(&self.shared.counters.consumer_contention);
                return Ok(EncoderPoll::Pending);
            }
            Err(TryLockError::Poisoned(_)) => {
                let error = internal_error(
                    "The encoder packet queue was poisoned by a panic, so its contents cannot be trusted."
                        .to_string(),
                );
                self.shared.fail(error.clone());
                return Err(error);
            }
        };
        match popped {
            Some(packet) => {
                self.shared.packets_held.fetch_sub(1, Ordering::AcqRel);
                self.shared
                    .bytes_held
                    .fetch_sub(packet.data.capacity() as u64, Ordering::AcqRel);
                EncoderCounters::bump(&self.shared.counters.packets_dequeued);
                EncoderCounters::add(
                    &self.shared.counters.bytes_dequeued,
                    packet.data.len() as u64,
                );
                self.shared.notify_worker();
                Ok(EncoderPoll::Packet(packet))
            }
            None => {
                if self.drained() || (self.stopped && self.stop_result.is_none()) {
                    Ok(EncoderPoll::Drained)
                } else {
                    Ok(EncoderPoll::Pending)
                }
            }
        }
    }

    fn stream_info(&self) -> &EncodedStreamInfo {
        &self.info
    }

    /// Idempotent, out-of-band drain request: it closes admission and signals the worker
    /// without waiting for a queue slot, even when the ingress queue is full.
    fn begin_drain(&mut self) -> Result<(), RecorderError> {
        self.terminal_error()?;
        if !self.control.drain.swap(true, Ordering::AcqRel) {
            EncoderCounters::bump(&self.shared.counters.drain_requests);
        }
        self.shared.notify_worker();
        Ok(())
    }

    /// Idempotent teardown/join on a control thread.
    ///
    /// Success requires a completed drain with every accepted input accounted for and no
    /// discarded packet. An early stop, a failed drain or any native failure still
    /// releases everything but reports the failure, so an incomplete stream can never
    /// look like a finished one.
    fn stop(&mut self) -> Result<(), RecorderError> {
        if !self.stopped {
            self.stopped = true;
            self.control.stop.store(true, Ordering::Release);
            self.shared.notify_worker();
            let mut failure = None;
            if let Some(mut guard) = self.worker.take()
                && !guard.stop_and_join()
            {
                failure = Some(internal_error(
                    "The hardware encoder worker thread panicked, so its native session may not have been released cleanly."
                        .to_string(),
                ));
            }
            let discarded = {
                let mut queue = match self.shared.packets.lock() {
                    Ok(queue) => queue,
                    Err(poison) => poison.into_inner(),
                };
                let packets = queue.drain();
                let count = packets.len();
                let bytes: u64 = packets
                    .iter()
                    .map(|packet| packet.data.capacity() as u64)
                    .sum();
                drop(packets);
                if count > 0 {
                    self.shared.packets_held.fetch_sub(count, Ordering::AcqRel);
                    self.shared.bytes_held.fetch_sub(bytes, Ordering::AcqRel);
                }
                count
            };
            if discarded > 0 {
                EncoderCounters::add(
                    &self.shared.counters.packets_discarded_on_stop,
                    discarded as u64,
                );
            }
            match self.shared.terminal_outcome() {
                Some(TerminalOutcome::Failed(error)) => {
                    if failure.is_none() {
                        failure = Some(error.clone());
                    }
                }
                Some(TerminalOutcome::Clean { drained: false }) if failure.is_none() => {
                    failure = Some(finalization_error(
                        "The encoder stopped without completing the stream.".to_string(),
                    ));
                }
                _ => {}
            }
            if let Some(error) = self.shared.terminal.get()
                && failure.is_none()
            {
                failure = Some(error.clone());
            }
            if failure.is_none() {
                if !self.control.drain.load(Ordering::Acquire) {
                    failure = Some(finalization_error(
                        "The encoder was stopped before it was asked to drain, so the stream is incomplete."
                            .to_string(),
                    ));
                } else if !self.shared.eos_acknowledged.load(Ordering::Acquire) {
                    failure = Some(finalization_error(
                        "The encoder stopped without end-of-stream acknowledgement, so the stream is incomplete."
                            .to_string(),
                    ));
                } else if discarded > 0 {
                    failure = Some(finalization_error(format!(
                        "{discarded} encoded packet(s) were still queued when the encoder stopped, so they were never delivered."
                    )));
                } else if self.shared.retained.load(Ordering::Acquire) != 0 {
                    failure = Some(finalization_error(format!(
                        "{} accepted capture lease(s) were still retained when the encoder stopped.",
                        self.shared.retained.load(Ordering::Acquire)
                    )));
                }
            }
            self.stop_result = failure;
        }
        match &self.stop_result {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl Drop for WindowsH264Encoder {
    fn drop(&mut self) {
        // Safety net only: cancels, joins and releases. It never detaches a live worker
        // and never recycles an in-flight resource by force.
        let _ = self.stop();
    }
}
