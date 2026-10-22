//! Recording quality, frame-rate, resolution and encoder policy (FRAME-0207).
//!
//! This module turns the canonical selections of a [`frame_contracts::RecordingRequest`]
//! plus the values a real hardware probe reported into one explicit [`RecordingPlan`]:
//!
//! - a documented bitrate target for Standard, High and Maximum, computed from the
//!   configured dimensions and the **nominal configuration rate** — never from a measured
//!   frame rate — and reconciled with the encoder's own reported limits and the H.264
//!   level ceiling, with every reconciliation recorded as a [`PlanDecision`];
//! - source-paced or manual frame rate, where a manual rate is a grid over the real
//!   captured timestamps and never a request to invent frames;
//! - native resolution, with an exact-size request either honoured because it already is
//!   the native size or refused outright, never silently recorded as native;
//! - automatic encoder selection among the candidates a real probe qualified, or an
//!   explicit `EncoderUnavailable` when the requested one is not among them.
//!
//! Nothing here measures anything, opens a device or claims a capability. The policy is a
//! pure function of its inputs, so a plan can be computed and inspected without hardware.

use frame_contracts::{
    Dimensions, EncoderPreference, ErrorCode, FpsMode, Quality, RecorderError, RecoveryStatus,
    Resolution,
};
use std::num::NonZeroU32;

/// The codec this policy plans for. The only implemented hardware path is H.264, and no
/// other codec is invented here.
pub const PLANNED_CODEC: &str = "h264";

/// A fixed-tier constraint of the current Windows adapter path, not a hardware probe.
/// Intel/AMD MF currently applies only Standard CBR/low-latency. Other vendors have
/// no known fixed-tier requirement here; their encoder admission is still checked.
pub fn windows_adapter_required_quality(vendor_id: u32) -> Option<Quality> {
    matches!(vendor_id, 0x8086 | 0x1002).then_some(Quality::Standard)
}

/// The smallest denominator searched when an `f64` rate is recovered as an exact rational.
/// 1001 covers the NTSC families (24000/1001, 30000/1001, 60000/1001).
const MAX_RATE_DENOMINATOR: u32 = 1001;
/// The largest deviation from the requested value that still counts as an exact rational.
const RATE_TOLERANCE_HZ: f64 = 0.001;
/// The keyframe interval, in seconds of the nominal configuration rate.
const KEYFRAME_INTERVAL_SECONDS: u32 = 2;
/// The bounded keyframe interval, in accepted frames.
const MIN_GOP_FRAMES: u32 = 1;
const MAX_GOP_FRAMES: u32 = 600;
/// The documented peak-to-average ratio of the VBR mode.
const VBR_PEAK_NUMERATOR: u32 = 3;
const VBR_PEAK_DENOMINATOR: u32 = 2;

fn invalid(message: impl Into<String>) -> RecorderError {
    RecorderError::invalid(message)
}

fn unsupported(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::UnsupportedCodec,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

fn encoder_unavailable(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::EncoderUnavailable,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

fn not_implemented(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

// ------------------------------------------------------------------ nominal rate

/// The exact nominal configuration rate a recording is initialized with.
///
/// It is always an explicit positive rational. It is the rate the encoder is *configured*
/// with — an enumerated display-mode rate or an explicitly requested manual rate — and it
/// is never a measured throughput. The stream's own pacing always comes from the real
/// captured timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NominalRate {
    num: NonZeroU32,
    den: NonZeroU32,
}

impl NominalRate {
    /// One explicit rational rate.
    pub fn new(num: NonZeroU32, den: NonZeroU32) -> Self {
        Self { num, den }
    }

    /// The numerator, in frames.
    pub fn num(&self) -> NonZeroU32 {
        self.num
    }

    /// The denominator, in seconds.
    pub fn den(&self) -> NonZeroU32 {
        self.den
    }

    /// The documented decimal value of the rational, for reporting only.
    pub fn frames_per_second(&self) -> f64 {
        f64::from(self.num.get()) / f64::from(self.den.get())
    }

    /// Recovers the requested value as an exact rational.
    ///
    /// The smallest rational with a denominator up to 1001 that reproduces the value is
    /// chosen; whole-hertz rates come back as `n/1` and the NTSC families as `n/1001`. A
    /// value that no such rational reproduces is reported instead of being rounded into a
    /// rate the user did not ask for.
    pub fn recover_exact(requested: f64) -> Result<Self, RecorderError> {
        if !requested.is_finite() || requested <= 0.0 {
            return Err(invalid(format!(
                "A frame rate of {requested} is not a usable positive rate."
            )));
        }
        let mut best: Option<(u32, u32, f64)> = None;
        for denominator in 1..=MAX_RATE_DENOMINATOR {
            let numerator = (requested * f64::from(denominator)).round();
            if numerator < 1.0 || numerator > f64::from(u32::MAX) {
                continue;
            }
            let error = (numerator / f64::from(denominator) - requested).abs();
            if best.is_none_or(|(_, _, best_error)| error < best_error) {
                best = Some((numerator as u32, denominator, error));
            }
        }
        let Some((numerator, denominator, error)) = best else {
            return Err(invalid(format!(
                "A frame rate of {requested} cannot be written as an exact rational."
            )));
        };
        if error > RATE_TOLERANCE_HZ {
            return Err(invalid(format!(
                "A frame rate of {requested} is farther than {RATE_TOLERANCE_HZ} Hz from any exact rational with a denominator up to {MAX_RATE_DENOMINATOR}, so it would be a guess."
            )));
        }
        let divisor = greatest_common_divisor(numerator, denominator);
        let num = NonZeroU32::new(numerator / divisor)
            .ok_or_else(|| invalid("The recovered frame rate has a zero numerator."))?;
        let den = NonZeroU32::new(denominator / divisor)
            .ok_or_else(|| invalid("The recovered frame rate has a zero denominator."))?;
        Ok(Self { num, den })
    }
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let next = left % right;
        left = right;
        right = next;
    }
    left.max(1)
}

// ------------------------------------------------------------------ bitrate policy

/// The documented target of each quality tier, in millibits per pixel per frame.
///
/// These are the policy's own documented targets for 8-bit 4:2:0 H.264 High. They are not
/// measurements and no frame is analysed to produce them: the same geometry and the same
/// nominal configuration rate always produce the same target. Standard 0.07, High 0.12 and
/// Maximum 0.20 bits per pixel per frame are the tier separation, and they are reported
/// with every plan.
fn target_millibits_per_pixel(quality: Quality) -> u32 {
    match quality {
        Quality::Standard => 70,
        Quality::High => 120,
        Quality::Maximum => 200,
    }
}

/// The documented bitrate target for one tier, geometry and nominal configuration rate.
///
/// `bitrate = width * height * nominal_fps * bits_per_pixel`, rounded up to whole bits per
/// second. No measured frame rate takes part: the only rate in the computation is the rate
/// the encoder is configured with.
pub fn documented_bitrate_bps(
    quality: Quality,
    dimensions: Dimensions,
    nominal_rate: NominalRate,
) -> Result<u32, RecorderError> {
    if dimensions.width == 0 || dimensions.height == 0 {
        return Err(invalid(
            "A bitrate plan needs positive dimensions.".to_string(),
        ));
    }
    let pixels = u128::from(dimensions.width) * u128::from(dimensions.height);
    let numerator = pixels
        * u128::from(nominal_rate.num.get())
        * u128::from(target_millibits_per_pixel(quality));
    let denominator = u128::from(nominal_rate.den.get()) * 1_000u128;
    let bits_per_second = numerator.div_ceil(denominator);
    u32::try_from(bits_per_second).map_err(|_| {
        unsupported(
            format!(
                "The documented {:?} target for {}x{} at {}/{} nominal is {bits_per_second} bit/s, which the encoder interface cannot express as a 32-bit bitrate.",
                quality,
                dimensions.width,
                dimensions.height,
                nominal_rate.num.get(),
                nominal_rate.den.get()
            ),
            "Choose a smaller geometry or a lower quality.",
        )
    })
}

/// One H.264 level's documented limits, from ITU-T H.264 Table A-1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H264Level {
    /// The level's name as the recommendation writes it.
    pub name: &'static str,
    /// MaxFS: the largest frame size, in macroblocks.
    pub max_frame_macroblocks: u32,
    /// MaxMBPS: the largest macroblock rate, in macroblocks per second.
    pub max_macroblocks_per_second: u32,
    /// MaxBR for the Baseline, Main and Extended profiles, in kbit/s.
    pub max_bitrate_kbps_baseline: u32,
}

impl H264Level {
    /// MaxBR for the High profile: 1.25 times the Baseline/Main column of Table A-1.
    pub fn max_bitrate_bps(&self) -> u32 {
        self.max_bitrate_kbps_baseline * 1_000 * 5 / 4
    }
}

/// ITU-T H.264 Table A-1, in ascending level order.
const H264_LEVELS: [H264Level; 20] = [
    H264Level {
        name: "1",
        max_frame_macroblocks: 99,
        max_macroblocks_per_second: 1_485,
        max_bitrate_kbps_baseline: 64,
    },
    H264Level {
        name: "1b",
        max_frame_macroblocks: 99,
        max_macroblocks_per_second: 1_485,
        max_bitrate_kbps_baseline: 128,
    },
    H264Level {
        name: "1.1",
        max_frame_macroblocks: 396,
        max_macroblocks_per_second: 3_000,
        max_bitrate_kbps_baseline: 192,
    },
    H264Level {
        name: "1.2",
        max_frame_macroblocks: 396,
        max_macroblocks_per_second: 6_000,
        max_bitrate_kbps_baseline: 384,
    },
    H264Level {
        name: "1.3",
        max_frame_macroblocks: 396,
        max_macroblocks_per_second: 11_880,
        max_bitrate_kbps_baseline: 768,
    },
    H264Level {
        name: "2",
        max_frame_macroblocks: 396,
        max_macroblocks_per_second: 11_880,
        max_bitrate_kbps_baseline: 2_000,
    },
    H264Level {
        name: "2.1",
        max_frame_macroblocks: 792,
        max_macroblocks_per_second: 19_800,
        max_bitrate_kbps_baseline: 4_000,
    },
    H264Level {
        name: "2.2",
        max_frame_macroblocks: 1_620,
        max_macroblocks_per_second: 20_250,
        max_bitrate_kbps_baseline: 4_000,
    },
    H264Level {
        name: "3",
        max_frame_macroblocks: 1_620,
        max_macroblocks_per_second: 40_500,
        max_bitrate_kbps_baseline: 10_000,
    },
    H264Level {
        name: "3.1",
        max_frame_macroblocks: 3_600,
        max_macroblocks_per_second: 108_000,
        max_bitrate_kbps_baseline: 14_000,
    },
    H264Level {
        name: "3.2",
        max_frame_macroblocks: 5_120,
        max_macroblocks_per_second: 216_000,
        max_bitrate_kbps_baseline: 20_000,
    },
    H264Level {
        name: "4",
        max_frame_macroblocks: 8_192,
        max_macroblocks_per_second: 245_760,
        max_bitrate_kbps_baseline: 20_000,
    },
    H264Level {
        name: "4.1",
        max_frame_macroblocks: 8_192,
        max_macroblocks_per_second: 245_760,
        max_bitrate_kbps_baseline: 50_000,
    },
    H264Level {
        name: "4.2",
        max_frame_macroblocks: 8_704,
        max_macroblocks_per_second: 522_240,
        max_bitrate_kbps_baseline: 50_000,
    },
    H264Level {
        name: "5",
        max_frame_macroblocks: 22_080,
        max_macroblocks_per_second: 589_824,
        max_bitrate_kbps_baseline: 135_000,
    },
    H264Level {
        name: "5.1",
        max_frame_macroblocks: 36_864,
        max_macroblocks_per_second: 983_040,
        max_bitrate_kbps_baseline: 240_000,
    },
    H264Level {
        name: "5.2",
        max_frame_macroblocks: 36_864,
        max_macroblocks_per_second: 2_073_600,
        max_bitrate_kbps_baseline: 240_000,
    },
    H264Level {
        name: "6",
        max_frame_macroblocks: 139_264,
        max_macroblocks_per_second: 4_177_920,
        max_bitrate_kbps_baseline: 240_000,
    },
    H264Level {
        name: "6.1",
        max_frame_macroblocks: 139_264,
        max_macroblocks_per_second: 8_355_840,
        max_bitrate_kbps_baseline: 480_000,
    },
    H264Level {
        name: "6.2",
        max_frame_macroblocks: 139_264,
        max_macroblocks_per_second: 16_711_680,
        max_bitrate_kbps_baseline: 800_000,
    },
];

/// The lowest documented H.264 level whose MaxFS and MaxMBPS cover this geometry and the
/// nominal configuration rate.
///
/// The macroblock rate is evaluated against the configuration rate, not against a measured
/// one. A geometry or rate that no documented level covers is reported rather than pushed
/// into a level the stream would violate. MaxDpbMbs (reference-picture capacity) is not
/// evaluated here and remains an open limit.
pub fn required_level(
    dimensions: Dimensions,
    nominal_rate: NominalRate,
) -> Result<H264Level, RecorderError> {
    let macroblocks_wide = u64::from(dimensions.width).div_ceil(16);
    let macroblocks_high = u64::from(dimensions.height).div_ceil(16);
    let frame_macroblocks = macroblocks_wide * macroblocks_high;
    let macroblock_rate = (frame_macroblocks * u64::from(nominal_rate.num.get()))
        .div_ceil(u64::from(nominal_rate.den.get()));
    for level in H264_LEVELS {
        if frame_macroblocks <= u64::from(level.max_frame_macroblocks)
            && macroblock_rate <= u64::from(level.max_macroblocks_per_second)
        {
            return Ok(level);
        }
    }
    Err(unsupported(
        format!(
            "A {}x{} stream at a nominal {}/{} needs {frame_macroblocks} macroblocks per frame and {macroblock_rate} macroblocks per second, which no documented H.264 level covers.",
            dimensions.width,
            dimensions.height,
            nominal_rate.num.get(),
            nominal_rate.den.get()
        ),
        "Choose a smaller geometry or a lower nominal rate.",
    ))
}

/// Reconciles a documented target with the level ceiling.
///
/// The applied bitrate is the target unless the target is above the ceiling, in which case
/// it is the ceiling and the caller records the clamp as an explicit decision.
pub fn apply_bitrate_ceiling(target_bps: u32, ceiling_bps: u32) -> (u32, bool) {
    if target_bps > ceiling_bps {
        (ceiling_bps, true)
    } else {
        (target_bps, false)
    }
}

/// The documented peak bitrate of the VBR mode: three halves of the average, never above
/// the level ceiling.
pub fn vbr_peak_bps(average_bps: u32, ceiling_bps: u32) -> u32 {
    let peak = average_bps.saturating_mul(VBR_PEAK_NUMERATOR) / VBR_PEAK_DENOMINATOR;
    peak.min(ceiling_bps)
}

/// The keyframe interval, in accepted frames, for one nominal configuration rate.
pub fn keyframe_interval_frames(nominal_rate: NominalRate) -> u32 {
    let frames = (u64::from(nominal_rate.num.get()) * u64::from(KEYFRAME_INTERVAL_SECONDS))
        .div_ceil(u64::from(nominal_rate.den.get()));
    u32::try_from(frames)
        .unwrap_or(MAX_GOP_FRAMES)
        .clamp(MIN_GOP_FRAMES, MAX_GOP_FRAMES)
}

// ------------------------------------------------------------- encoder candidates

/// The limits one real encoder candidate reported for its H.264 path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderLimits {
    /// The largest width the candidate reported.
    pub max_width: u32,
    /// The largest height the candidate reported.
    pub max_height: u32,
    /// Whether the candidate reported the CBR rate-control mode.
    pub cbr: bool,
    /// Whether the candidate reported the VBR rate-control mode.
    pub vbr: bool,
}

/// One encoder candidate a real probe qualified.
///
/// A candidate exists only because a real session was created for it; this crate never
/// synthesises a QSV, AMF, VideoToolbox or software candidate, so an empty list is an
/// unavailable encoder rather than a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderCandidate {
    pub id: String,
    pub codec: String,
    pub hardware: bool,
    pub limits: EncoderLimits,
}

impl EncoderCandidate {
    fn admits(&self, dimensions: Dimensions) -> bool {
        self.limits.max_width >= dimensions.width && self.limits.max_height >= dimensions.height
    }
}

/// The candidate a plan selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedEncoder {
    pub id: String,
    pub codec: String,
    pub hardware: bool,
    pub limits: EncoderLimits,
}

/// The rate-control mode a plan asks the encoder to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    Cbr,
    Vbr,
    Cqp,
}

/// The encoder preset/tuning pair a plan asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderPreset {
    /// Native lossless transform bypass, never substituted with a bitrate tier.
    Lossless,
    /// The low-latency preset, used by the Standard tier.
    LowLatency,
    /// The high-quality preset, used by the High tier.
    HighQuality,
    /// The highest-quality preset, used by the Maximum tier.
    MaximumQuality,
}

fn quality_configuration(quality: Quality) -> (RateControl, EncoderPreset) {
    match quality {
        Quality::Standard => (RateControl::Cbr, EncoderPreset::LowLatency),
        Quality::High => (RateControl::Vbr, EncoderPreset::HighQuality),
        Quality::Maximum => (RateControl::Vbr, EncoderPreset::MaximumQuality),
    }
}

/// The frame-rate plan of one recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRatePlan {
    /// Every real captured frame continues to the encoder; the source paces the stream
    /// and no software ceiling is applied.
    SourcePaced,
    /// Real captured frames are admitted on a fixed grid of this explicit rational.
    Fixed(NominalRate),
}

/// The resolution plan of one recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionPlan {
    /// The capture surface's own size; no frame is scaled and none is assumed.
    Native(Dimensions),
}

/// What the policy did, in one bounded line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionCode {
    AutomaticSettings,
    ManualBitrate,
    ManualEncoderSettings,
    AutoEncoderSelected,
    ExplicitEncoderSelected,
    SourcePacedFrameRate,
    ManualFrameRate,
    NativeResolution,
    ExactResolutionHonored,
    DocumentedBitrateTarget,
    BitrateClampedToLevelCeiling,
    RateControlCbrFallback,
    ThroughputPresetSelected,
}

/// One explicit policy decision, carried with the plan as evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDecision {
    pub code: DecisionCode,
    pub detail: String,
}

/// The plan of one recording: what was selected, what was computed and why.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingPlan {
    pub encoder: SelectedEncoder,
    pub resolution: ResolutionPlan,
    pub frame_rate: FrameRatePlan,
    /// The nominal configuration rate the encoder is initialized with.
    pub nominal_rate: NominalRate,
    pub quality: Quality,
    pub rate_control: RateControl,
    pub preset: EncoderPreset,
    /// The documented target, the applied bitrate and the applied peak.
    pub calculated_bitrate_bps: u32,
    pub bitrate_bps: u32,
    pub peak_bitrate_bps: u32,
    /// The documented H.264 level the geometry and the configuration rate require.
    pub level: H264Level,
    /// The level's ceiling that bounded the applied bitrate.
    pub level_ceiling_bps: u32,
    pub gop_frames: u32,
    pub encoder_overrides: frame_contracts::VideoEncoderOverrides,
    pub decisions: Vec<PlanDecision>,
}

/// Everything the policy reads to build one plan.
#[derive(Clone, Debug)]
pub struct PlanInputs<'a> {
    pub quality: Quality,
    pub resolution: Resolution,
    pub fps_mode: FpsMode,
    pub encoder: EncoderPreference,
    /// The capture surface's own geometry.
    pub native: Dimensions,
    /// The nominal configuration rate for a source-paced recording.
    pub nominal_rate: NominalRate,
    /// The candidates a real probe qualified.
    pub candidates: &'a [EncoderCandidate],
}

/// Builds the plan of one recording from existing selections and reported hardware values.
pub fn plan_recording(inputs: &PlanInputs<'_>) -> Result<RecordingPlan, RecorderError> {
    plan_recording_for_codec(inputs, PLANNED_CODEC)
}

/// The same frozen plan for an explicitly supported native codec. The existing
/// H.264 entry point and selection order remain unchanged.
pub fn plan_recording_for_codec(
    inputs: &PlanInputs<'_>,
    codec: &'static str,
) -> Result<RecordingPlan, RecorderError> {
    plan_recording_with_assessments(inputs, codec, &[])
}

/// Presets resolve through the existing planner. Fixed FPS, explicit encoder,
/// codec, quality and bitrate remain authoritative. A running plan never changes.
pub fn plan_optimized_recording(
    inputs: &PlanInputs<'_>,
    options: Option<&frame_contracts::RecordingOptimization>,
    default_codec: &'static str,
    assessments: &[(String, crate::assessment::EncoderAssessment)],
) -> Result<RecordingPlan, RecorderError> {
    use frame_contracts::{RecordingCodec, RecordingPreset};
    let Some(options) = options else {
        return plan_recording_for_codec(inputs, default_codec);
    };
    let automatic = options.preset != RecordingPreset::Custom;
    let codecs: &[&str] = match options.codec {
        RecordingCodec::H264 => &["h264"],
        RecordingCodec::Hevc => &["hevc"],
        RecordingCodec::Av1 => &["av1"],
        RecordingCodec::Auto if options.preset == RecordingPreset::Lossless => &["h264"],
        RecordingCodec::Auto if default_codec == "hevc" => &["hevc", "av1"],
        RecordingCodec::Auto if automatic => &["h264", "av1"],
        RecordingCodec::Auto => &[default_codec],
    };
    let mut plans = Vec::new();
    let mut last_error = None;
    for &codec in codecs {
        for candidate in inputs
            .candidates
            .iter()
            .filter(|c| c.codec == codec && c.hardware)
        {
            if matches!(&inputs.encoder, EncoderPreference::Explicit(id) if id != &candidate.id) {
                continue;
            }
            let mut resolved = inputs.clone();
            resolved.encoder = EncoderPreference::Explicit(candidate.id.clone());
            if automatic && options.automatic_quality {
                resolved.quality =
                    if candidate.id.contains("-mf-") || options.preset == RecordingPreset::Gaming {
                        Quality::Standard
                    } else {
                        Quality::High
                    };
            }
            let measurement = assessments
                .iter()
                .find(|(id, _)| id == &candidate.id)
                .map(|(_, v)| *v);
            // Low measured headroom or latency chooses the lighter quality preset;
            // bitrate still comes from the same documented geometry/rate policy.
            if automatic
                && options.automatic_quality
                && measurement.is_some_and(|m| {
                    m.capacity_millihz().is_some_and(|n| {
                        n < (inputs.nominal_rate.frames_per_second() * 1200.0) as u64
                    }) || m.latency_p95_ns.is_some_and(|n| {
                        n as f64 > 1_000_000_000.0 / inputs.nominal_rate.frames_per_second()
                    })
                })
            {
                resolved.quality = Quality::Standard;
            }
            if automatic
                && inputs.fps_mode == FpsMode::AutoMatchSource
                && let Some(capacity) = measurement.and_then(|m| m.capacity_millihz())
            {
                let source = inputs.nominal_rate.frames_per_second();
                let sustainable = capacity as f64 / 1000.0;
                if sustainable < source && sustainable > 0.0 {
                    // Millihertz rational, not a list of arbitrary FPS tiers.
                    resolved.fps_mode = FpsMode::Fixed(sustainable);
                }
            }
            // Measurements choose automatic settings and rank alternatives. They
            // never veto a manual rate or explicit encoder; native admission does.
            let result = plan_recording_for_codec(&resolved, codec).and_then(|mut plan| {
                if options.preset == RecordingPreset::Lossless {
                    if codec != "h264" || !candidate.id.starts_with("nvidia-nvenc-h264-d3d11:") {
                        return Err(encoder_unavailable("Lossless recording requires a qualified native H.264 lossless encoder.".into(), "Choose Desktop or Gaming on this adapter."));
                    }
                    plan.preset = EncoderPreset::Lossless;
                    plan.bitrate_bps = 0;
                    plan.peak_bitrate_bps = 0;
                    plan.level_ceiling_bps = u32::MAX;
                }
                if let Some(bitrate) = options.bitrate_bps {
                    if plan.preset == EncoderPreset::Lossless {
                        return Err(invalid("Lossless encoding uses QP 0. Clear the bitrate override or choose Custom."));
                    }
                    if bitrate == 0 || bitrate > plan.level_ceiling_bps {
                        return Err(invalid("The manual bitrate is zero or exceeds this codec level's limit."));
                    }
                    plan.bitrate_bps = bitrate;
                    plan.peak_bitrate_bps = if plan.rate_control == RateControl::Vbr { vbr_peak_bps(bitrate, plan.level_ceiling_bps) } else { bitrate };
                    plan.decisions.push(PlanDecision { code: DecisionCode::ManualBitrate, detail: format!("Manual bitrate: {bitrate} bit/s.") });
                }
                plan.decisions.push(PlanDecision { code: DecisionCode::AutomaticSettings, detail: format!("{:?}: {} / {:?}, {}/{} FPS; {} bit/s.{}", options.preset, codec, plan.quality, plan.nominal_rate.num(), plan.nominal_rate.den(), plan.bitrate_bps, if options.preset == RecordingPreset::Lossless { " Lossless YUV 4:4:4 encoding; RGB colour conversion remains." } else { "" }) });
                let advanced = options.encoder_overrides;
                advanced.validate()?;
                if !advanced.is_empty() {
                    if !candidate.id.starts_with("nvidia-nvenc-") || !matches!(codec, "h264" | "av1") || options.preset == RecordingPreset::Lossless {
                        return Err(invalid("Native Advanced overrides require the implemented SDR H.264/AV1 NVENC path."));
                    }
                    plan.encoder_overrides = advanced;
                    if advanced.preset.is_some() || advanced.tuning.is_some() {
                        plan.decisions.retain(|d| d.code != DecisionCode::ThroughputPresetSelected);
                    }
                    if let Some(gop) = advanced.gop_frames { plan.gop_frames = gop; }
                    if let Some(rc) = advanced.rate_control {
                        plan.rate_control = match rc {
                            frame_contracts::VideoRateControl::Cbr if candidate.limits.cbr => RateControl::Cbr,
                            frame_contracts::VideoRateControl::Vbr if candidate.limits.vbr => RateControl::Vbr,
                            frame_contracts::VideoRateControl::Cqp => RateControl::Cqp,
                            _ => return Err(invalid("The exact rate-control mode is unavailable; no fallback is allowed.")),
                        };
                        plan.decisions.retain(|d| d.code != DecisionCode::RateControlCbrFallback);
                        if plan.rate_control == RateControl::Cqp {
                            if options.bitrate_bps.is_some() { return Err(invalid("CQP does not accept a bitrate target.")); }
                            plan.bitrate_bps = 0; plan.peak_bitrate_bps = 0;
                        } else {
                            plan.peak_bitrate_bps = if plan.rate_control == RateControl::Vbr { vbr_peak_bps(plan.bitrate_bps, plan.level_ceiling_bps) } else { plan.bitrate_bps };
                        }
                    }
                    plan.decisions.push(PlanDecision { code: DecisionCode::ManualEncoderSettings,
                        detail: format!("Explicit native encoder intent: {advanced:?}; final {:?}, target {} bit/s, peak {} bit/s, GOP {} frames. Native initialization must accept this exact combination.", plan.rate_control, plan.bitrate_bps, plan.peak_bitrate_bps, plan.gop_frames) });
                }
                Ok(plan)
            });
            match result {
                Ok(plan) => plans.push((plan, measurement)),
                Err(error) => last_error = Some(error),
            }
        }
    }
    // Prefer candidates without measured failures, then source rate and measured
    // cost. If every calibration failed, native qualification remains authoritative.
    plans
        .into_iter()
        .enumerate()
        .max_by_key(|(i, (p, m))| {
            (
                !m.is_some_and(|m| m.failures != 0),
                (p.nominal_rate.frames_per_second() * 1000.0) as u64,
                crate::assessment::rank(
                    &EncoderCandidate {
                        id: p.encoder.id.clone(),
                        codec: p.encoder.codec.clone(),
                        hardware: p.encoder.hardware,
                        limits: p.encoder.limits,
                    },
                    *m,
                    p.nominal_rate,
                    p.quality,
                ),
                std::cmp::Reverse(*i),
            )
        })
        .map(|(_, (p, _))| p)
        .ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                encoder_unavailable(
                    "No qualified encoder supports the requested recording settings.".into(),
                    "Choose an available codec or automatic selection.",
                )
            })
        })
}

/// Uses only current, geometry/backend-matched measured inputs supplied by the
/// caller. The plan stays immutable after selection; this is not runtime adaptation.
pub fn plan_recording_with_assessments(
    inputs: &PlanInputs<'_>,
    codec: &'static str,
    assessments: &[(String, crate::assessment::EncoderAssessment)],
) -> Result<RecordingPlan, RecorderError> {
    if codec != PLANNED_CODEC && codec != "hevc" && codec != "av1" {
        return Err(not_implemented(
            format!("No recording policy exists for codec {codec}."),
            "Select H.264, HEVC Main10 or AV1 on a qualified device.",
        ));
    }
    let mut decisions = Vec::new();

    let (frame_rate, nominal_rate) = match inputs.fps_mode {
        FpsMode::AutoMatchSource => {
            decisions.push(PlanDecision {
                code: DecisionCode::SourcePacedFrameRate,
                detail: format!(
                    "source-paced: every real captured frame continues to the encoder, configured at the nominal {}/{} rate",
                    inputs.nominal_rate.num.get(),
                    inputs.nominal_rate.den.get()
                ),
            });
            (FrameRatePlan::SourcePaced, inputs.nominal_rate)
        }
        FpsMode::Fixed(requested) => {
            let rate = NominalRate::recover_exact(requested)?;
            decisions.push(PlanDecision {
                code: DecisionCode::ManualFrameRate,
                detail: format!(
                    "manual {}/{} (requested {requested}): real captured frames are admitted on this grid and the encoder is configured with it; the source's own pace still decides how many frames exist",
                    rate.num.get(),
                    rate.den.get()
                ),
            });
            (FrameRatePlan::Fixed(rate), rate)
        }
    };

    let resolution = match inputs.resolution {
        Resolution::Native => {
            decisions.push(PlanDecision {
                code: DecisionCode::NativeResolution,
                detail: format!(
                    "native {}x{}: no frame is scaled",
                    inputs.native.width, inputs.native.height
                ),
            });
            ResolutionPlan::Native(inputs.native)
        }
        Resolution::Exact(requested) => {
            if requested != inputs.native {
                return Err(not_implemented(
                    format!(
                        "A {}x{} recording was requested, but the capture surface is {}x{} and this path never scales a frame: the request is refused instead of being recorded at the native size.",
                        requested.width,
                        requested.height,
                        inputs.native.width,
                        inputs.native.height
                    ),
                    "Choose the native resolution, or a build that scales in the capture path.",
                ));
            }
            decisions.push(PlanDecision {
                code: DecisionCode::ExactResolutionHonored,
                detail: format!(
                    "the requested {}x{} is the capture surface's own size, so nothing is scaled",
                    requested.width, requested.height
                ),
            });
            ResolutionPlan::Native(requested)
        }
    };

    let encoder = select_encoder(
        &inputs.encoder,
        inputs.native,
        inputs.candidates,
        codec,
        nominal_rate,
        inputs.quality,
        assessments,
    )?;
    if encoder.0.id.contains("-mf-h264-d3d11:") && inputs.quality != Quality::Standard {
        return Err(encoder_unavailable(
            format!(
                "The selected Media Foundation encoder {} supports the verified Standard CBR/low-latency tier only; {:?} has no applied preset and rate-control readback.",
                encoder.0.id, inputs.quality
            ),
            "Choose Standard quality or a qualified encoder supporting this quality tier.",
        ));
    }
    decisions.push(encoder.1);

    let calculated_bitrate_bps =
        documented_bitrate_bps(inputs.quality, inputs.native, nominal_rate)?;
    let level = if codec == PLANNED_CODEC {
        required_level(inputs.native, nominal_rate)?
    } else {
        // This field describes AVC only; other codecs validate their own levels natively.
        H264_LEVELS[H264_LEVELS.len() - 1]
    };
    // The H.264 level ceiling does not describe HEVC. NVENC validates the HEVC
    // target at initialization; retain the documented bitrate without claiming
    // an H.264 level limit applies to it.
    let level_ceiling_bps = if codec == PLANNED_CODEC {
        level.max_bitrate_bps()
    } else {
        u32::MAX
    };
    let (bitrate_bps, clamped) = apply_bitrate_ceiling(calculated_bitrate_bps, level_ceiling_bps);
    decisions.push(PlanDecision {
        code: DecisionCode::DocumentedBitrateTarget,
        detail: format!(
            "{:?} at {}x{} and a nominal {}/{} = {calculated_bitrate_bps} bit/s ({} millibits per pixel per frame)",
            inputs.quality,
            inputs.native.width,
            inputs.native.height,
            nominal_rate.num.get(),
            nominal_rate.den.get(),
            target_millibits_per_pixel(inputs.quality)
        ),
    });
    if clamped && codec == PLANNED_CODEC {
        decisions.push(PlanDecision {
            code: DecisionCode::BitrateClampedToLevelCeiling,
            detail: format!(
                "the documented target {calculated_bitrate_bps} bit/s is above the H.264 level {} ceiling of {level_ceiling_bps} bit/s, so {level_ceiling_bps} bit/s is applied",
                level.name
            ),
        });
    }

    let (desired_rate_control, mut preset) = quality_configuration(inputs.quality);
    if nominal_rate.frames_per_second() >= 120.0 && preset != EncoderPreset::LowLatency {
        preset = EncoderPreset::LowLatency;
        decisions.push(PlanDecision {
            code: DecisionCode::ThroughputPresetSelected,
            detail: "High frame-rate recording uses the hardware low-latency preset with no lookahead/B frames; the selected quality tier's bitrate and rate-control target are retained. Native initialization must accept the exact configuration.".into(),
        });
    }
    let rate_control = if desired_rate_control == RateControl::Vbr && !encoder.0.limits.vbr {
        decisions.push(PlanDecision {
            code: DecisionCode::RateControlCbrFallback,
            detail: format!(
                "the selected encoder {} did not report the VBR mode this tier asks for, so CBR is applied at the same bitrate",
                encoder.0.id
            ),
        });
        RateControl::Cbr
    } else {
        desired_rate_control
    };
    let peak_bitrate_bps = match rate_control {
        RateControl::Cbr | RateControl::Cqp => bitrate_bps,
        RateControl::Vbr => vbr_peak_bps(bitrate_bps, level_ceiling_bps),
    };

    Ok(RecordingPlan {
        encoder: encoder.0,
        resolution,
        frame_rate,
        nominal_rate,
        quality: inputs.quality,
        rate_control,
        preset,
        calculated_bitrate_bps,
        bitrate_bps,
        peak_bitrate_bps,
        level,
        level_ceiling_bps,
        gop_frames: keyframe_interval_frames(nominal_rate),
        encoder_overrides: Default::default(),
        decisions,
    })
}

/// Selects one candidate, or reports that none is available.
///
/// Only candidates a real probe qualified, that are hardware and that carry the planned
/// codec take part. Automatic selection ranks supplied measurements and uses the
/// probe's order for ties. Reported dimensions must cover the geometry; explicit selection must name
/// one of those candidates, and a name that is not among them is an explicit
/// `EncoderUnavailable` rather than a fallback.
fn select_encoder(
    preference: &EncoderPreference,
    dimensions: Dimensions,
    candidates: &[EncoderCandidate],
    codec: &'static str,
    rate: NominalRate,
    quality: Quality,
    assessments: &[(String, crate::assessment::EncoderAssessment)],
) -> Result<(SelectedEncoder, PlanDecision), RecorderError> {
    let assessment = |candidate: &EncoderCandidate| {
        assessments
            .iter()
            .find(|(id, _)| id == &candidate.id)
            .map(|(_, value)| *value)
    };
    let usable: Vec<&EncoderCandidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate.hardware
                && candidate.codec == codec
                && crate::assessment::rank(candidate, assessment(candidate), rate, quality)
                    .is_some()
        })
        .collect();
    let available = || {
        if candidates.is_empty() {
            "no candidate was reported".to_string()
        } else {
            candidates
                .iter()
                .take(8)
                .map(|candidate| {
                    format!(
                        "{} ({} , hardware={}, up to {}x{})",
                        candidate.id,
                        candidate.codec,
                        candidate.hardware,
                        candidate.limits.max_width,
                        candidate.limits.max_height
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
    };
    if usable.is_empty() {
        return Err(encoder_unavailable(
            format!(
                "No usable {} hardware encoder is available: {}. This path has no software, QSV or AMF encoder to fall back to.",
                codec,
                available()
            ),
            "Install a supported NVIDIA driver and probe the encoders again.",
        ));
    }
    let selected = match preference {
        EncoderPreference::Auto => usable
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, candidate)| candidate.admits(dimensions))
            .max_by_key(|(index, candidate)| (
                crate::assessment::rank(candidate, assessment(candidate), rate, quality),
                std::cmp::Reverse(*index),
            ))
            .map(|(_, candidate)| candidate)
            .ok_or_else(|| {
                unsupported(
                    format!(
                        "The available {} hardware encoder(s) do not cover {}x{}: {}.",
                        codec,
                        dimensions.width,
                        dimensions.height,
                        available()
                    ),
                    "Choose a smaller capture geometry.",
                )
            })?,
        EncoderPreference::Explicit(id) => usable
            .iter()
            .copied()
            .find(|candidate| candidate.id == *id)
            .ok_or_else(|| {
                encoder_unavailable(
                    format!(
                        "The requested encoder {id} is not among the qualified {codec} hardware encoders: {}.",
                        available()
                    ),
                    "Choose one of the listed encoders or automatic selection.",
                )
            })?,
    };
    if !selected.admits(dimensions) {
        return Err(unsupported(
            format!(
                "The selected encoder {} reports limits of {}x{}, which do not cover the {}x{} capture surface.",
                selected.id,
                selected.limits.max_width,
                selected.limits.max_height,
                dimensions.width,
                dimensions.height
            ),
            "Choose a smaller capture geometry.",
        ));
    }
    let mut decision = match preference {
        EncoderPreference::Auto => PlanDecision {
            code: DecisionCode::AutoEncoderSelected,
            detail: format!(
                "automatic selection chose {} among the {} qualified hardware candidate(s); its reported limits are {}x{} with cbr={} vbr={}",
                selected.id,
                usable.len(),
                selected.limits.max_width,
                selected.limits.max_height,
                selected.limits.cbr,
                selected.limits.vbr
            ),
        },
        EncoderPreference::Explicit(id) => PlanDecision {
            code: DecisionCode::ExplicitEncoderSelected,
            detail: format!("the requested encoder {id} is qualified for this geometry"),
        },
    };
    if let Some(value) = assessment(selected)
        && let Some(throughput) = value.throughput_millihz()
    {
        decision.detail.push_str(&format!(
            "; measured synthetic throughput {:.3} FPS across {} completed frames; source-application impact {}. Short calibration is not sustained capture acceptance",
            throughput as f64 / 1000.0, value.completed_frames,
            if value.source_cost_p95_ns.is_some() { "measured" } else { "unmeasured" }
        ));
    }
    Ok((
        SelectedEncoder {
            id: selected.id.clone(),
            codec: selected.codec.clone(),
            hardware: selected.hardware,
            limits: selected.limits,
        },
        decision,
    ))
}

// ------------------------------------------------------------------ frozen plan

/// One recording session's plan, frozen before capture starts.
///
/// A live recording keeps the plan it started with: this type owns the plan and offers no
/// way to change or recompute it. A settings file that changes on disk, or a selection the
/// user changes while a recording is running, therefore cannot alter what the running
/// session encodes.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionPlan {
    plan: RecordingPlan,
}

impl SessionPlan {
    /// Freezes one plan for the lifetime of a recording session.
    pub fn freeze(plan: RecordingPlan) -> Self {
        Self { plan }
    }

    /// The plan this session is encoding with.
    pub fn plan(&self) -> &RecordingPlan {
        &self.plan
    }

    /// The plan, once the session has finished with it.
    pub fn into_plan(self) -> RecordingPlan {
        self.plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(num: u32, den: u32) -> NominalRate {
        NominalRate::new(NonZeroU32::new(num).unwrap(), NonZeroU32::new(den).unwrap())
    }

    fn candidate(id: &str, max_width: u32, max_height: u32, vbr: bool) -> EncoderCandidate {
        EncoderCandidate {
            id: id.to_string(),
            codec: PLANNED_CODEC.to_string(),
            hardware: true,
            limits: EncoderLimits {
                max_width,
                max_height,
                cbr: true,
                vbr,
            },
        }
    }

    #[test]
    fn mf_quality_is_refused_before_a_recording_plan_is_frozen() {
        for (vendor, encoder) in [
            (0x8086, "intel-mf-h264-d3d11:luid=0000000000000001"),
            (0x1002, "amd-mf-h264-d3d11:luid=0000000000000002"),
        ] {
            let candidates = vec![candidate(encoder, 1920, 1080, false)];
            let mut inputs = PlanInputs {
                quality: windows_adapter_required_quality(vendor).expect("known fixed tier"),
                resolution: Resolution::Native,
                fps_mode: FpsMode::AutoMatchSource,
                encoder: EncoderPreference::Auto,
                native: Dimensions {
                    width: 1920,
                    height: 1080,
                },
                nominal_rate: rate(60, 1),
                candidates: &candidates,
            };
            let standard = plan_recording(&inputs).expect("required MF tier");
            assert_eq!(standard.rate_control, RateControl::Cbr);
            assert_eq!(standard.preset, EncoderPreset::LowLatency);
            for quality in [Quality::High, Quality::Maximum] {
                inputs.quality = quality;
                assert_eq!(
                    plan_recording(&inputs).unwrap_err().code,
                    ErrorCode::EncoderUnavailable
                );
            }
        }
        assert_eq!(windows_adapter_required_quality(0x10DE), None);
        assert_eq!(windows_adapter_required_quality(0), None);
    }

    #[test]
    fn hdr_plan_selects_only_hevc_and_does_not_apply_an_h264_bitrate_ceiling() {
        let native = Dimensions {
            width: 3840,
            height: 2160,
        };
        let candidates = vec![
            candidate("nvidia-nvenc-h264-d3d11:RTX", 4096, 2160, true),
            EncoderCandidate {
                id: "nvidia-nvenc-hevc-main10-d3d11:RTX".into(),
                codec: "hevc".into(),
                hardware: true,
                limits: EncoderLimits {
                    max_width: 4096,
                    max_height: 2160,
                    cbr: true,
                    vbr: true,
                },
            },
        ];
        let inputs = PlanInputs {
            quality: Quality::Maximum,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native,
            nominal_rate: rate(60, 1),
            candidates: &candidates,
        };
        let hdr = plan_recording_for_codec(&inputs, "hevc").unwrap();
        assert_eq!(hdr.encoder.codec, "hevc");
        assert_eq!(hdr.encoder.id, "nvidia-nvenc-hevc-main10-d3d11:RTX");
        assert_eq!(hdr.bitrate_bps, hdr.calculated_bitrate_bps);
        assert_eq!(hdr.level_ceiling_bps, u32::MAX);
        assert_eq!(plan_recording(&inputs).unwrap().encoder.codec, "h264");
    }

    #[test]
    fn av1_plan_uses_only_qualified_candidates_and_keeps_h264_auto() {
        let mut av1 = candidate("qualified-av1", 4096, 2160, true);
        av1.codec = "av1".into();
        let candidates = vec![candidate("compatible-h264", 4096, 2160, true), av1];
        let mut inputs = PlanInputs {
            quality: Quality::Standard,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Explicit("qualified-av1".into()),
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: rate(60, 1),
            candidates: &candidates,
        };
        assert_eq!(
            plan_recording_for_codec(&inputs, "av1").unwrap().encoder.id,
            "qualified-av1"
        );
        inputs.encoder = EncoderPreference::Explicit("unsupported-main10".into());
        assert!(plan_recording_for_codec(&inputs, "av1").is_err());
        inputs.encoder = EncoderPreference::Auto;
        assert_eq!(plan_recording(&inputs).unwrap().encoder.codec, "h264");
    }

    /// The documented tier targets are fixed by geometry and the nominal configuration
    /// rate, and the level ceiling bounds what is applied.
    #[test]
    fn documented_targets_and_the_level_ceiling_bound_the_applied_bitrate() {
        let full_hd = Dimensions {
            width: 1920,
            height: 1080,
        };
        let sixty = rate(60, 1);
        assert_eq!(
            documented_bitrate_bps(Quality::Standard, full_hd, sixty).unwrap(),
            8_709_120
        );
        assert_eq!(
            documented_bitrate_bps(Quality::High, full_hd, sixty).unwrap(),
            14_929_920
        );
        assert_eq!(
            documented_bitrate_bps(Quality::Maximum, full_hd, sixty).unwrap(),
            24_883_200
        );
        // The NTSC rate is a configuration value, not a measurement.
        assert_eq!(
            documented_bitrate_bps(Quality::High, full_hd, rate(60_000, 1_001)).unwrap(),
            14_915_005
        );
        let level = required_level(full_hd, sixty).unwrap();
        assert_eq!(level.name, "4.2");
        assert_eq!(level.max_bitrate_bps(), 62_500_000);

        assert_eq!(
            apply_bitrate_ceiling(14_929_920, 62_500_000),
            (14_929_920, false)
        );
        assert_eq!(
            apply_bitrate_ceiling(90_000_000, 62_500_000),
            (62_500_000, true)
        );
        assert_eq!(vbr_peak_bps(14_929_920, 62_500_000), 22_394_880);
        assert_eq!(vbr_peak_bps(60_000_000, 62_500_000), 62_500_000);
    }

    /// An automatic plan uses the candidates it was given, refuses a geometry the
    /// hardware does not cover, refuses scaling instead of recording native, and freezes
    /// the plan it produced.
    #[test]
    fn automatic_planning_is_explicit_and_frozen() {
        let native = Dimensions {
            width: 2560,
            height: 1600,
        };
        // The first candidate cannot cover this surface, so automatic selection must move
        // on to the one that can instead of recording something the hardware refused.
        let candidates = vec![
            candidate("nvidia-nvenc-h264-d3d11:narrow", 1920, 1080, true),
            candidate("nvidia-nvenc-h264-d3d11:RTX", 4096, 2160, true),
        ];
        let inputs = PlanInputs {
            quality: Quality::High,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native,
            nominal_rate: rate(180, 1),
            candidates: &candidates,
        };
        let plan = plan_recording(&inputs).unwrap();
        assert_eq!(plan.encoder.id, "nvidia-nvenc-h264-d3d11:RTX");
        assert_eq!(plan.rate_control, RateControl::Vbr);
        assert_eq!(plan.preset, EncoderPreset::LowLatency);
        assert_eq!(plan.frame_rate, FrameRatePlan::SourcePaced);
        assert_eq!(plan.calculated_bitrate_bps, plan.bitrate_bps);
        assert_eq!(plan.gop_frames, 360);
        let session = SessionPlan::freeze(plan.clone());
        assert_eq!(session.plan(), &plan);
        // A frozen session plan is not a view over mutable selections: planning something
        // else cannot change what a running session reports.
        let mut other = inputs.clone();
        other.quality = Quality::Standard;
        let other_plan = plan_recording(&other).unwrap();
        assert_ne!(other_plan.bitrate_bps, plan.bitrate_bps);
        assert_eq!(session.plan().quality, Quality::High);
        assert_eq!(session.plan().rate_control, RateControl::Vbr);
        assert_eq!(session.plan().bitrate_bps, plan.bitrate_bps);

        // A candidate whose reported limits do not cover the surface is an explicit
        // refusal, not a silent native or scaled recording.
        let small = vec![candidate("nvenc", 1920, 1080, true)];
        let mut limited = inputs.clone();
        limited.candidates = &small;
        assert_eq!(
            plan_recording(&limited).unwrap_err().code,
            ErrorCode::UnsupportedCodec
        );

        // A geometry the hardware does not cover at all, and a scaling request.
        let mut scaling = inputs.clone();
        scaling.resolution = Resolution::Exact(Dimensions {
            width: 1280,
            height: 720,
        });
        assert_eq!(
            plan_recording(&scaling).unwrap_err().code,
            ErrorCode::NotImplemented
        );
        let none: Vec<EncoderCandidate> = vec![];
        let mut absent = inputs.clone();
        absent.candidates = &none;
        assert_eq!(
            plan_recording(&absent).unwrap_err().code,
            ErrorCode::EncoderUnavailable
        );
        let mut explicit = inputs;
        explicit.encoder = EncoderPreference::Explicit("amd-amf-h264".to_string());
        assert_eq!(
            plan_recording(&explicit).unwrap_err().code,
            ErrorCode::EncoderUnavailable
        );
    }
    #[test]
    fn high_fps_keeps_manual_admission_and_quality_bitrate() {
        let candidates = vec![candidate("nvenc", 4096, 2160, true)];
        let mut inputs = PlanInputs {
            quality: Quality::Maximum,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: rate(240, 1),
            candidates: &candidates,
        };
        for fps in [120, 144, 240, 480, 960] {
            inputs.nominal_rate = rate(fps, 1);
            let plan = plan_recording(&inputs).unwrap();
            assert_eq!(plan.preset, EncoderPreset::LowLatency);
            assert_eq!(plan.nominal_rate, rate(fps, 1));
            assert_eq!(
                plan.calculated_bitrate_bps,
                documented_bitrate_bps(Quality::Maximum, inputs.native, rate(fps, 1)).unwrap()
            );
        }
        inputs.nominal_rate = rate(144, 1);
        inputs.fps_mode = FpsMode::Fixed(240.0);
        assert_eq!(plan_recording(&inputs).unwrap().nominal_rate, rate(240, 1));
        inputs.fps_mode = FpsMode::Fixed(120.0);
        assert_eq!(plan_recording(&inputs).unwrap().nominal_rate, rate(120, 1));
    }

    #[test]
    fn optimizer_uses_capacity_and_codec_fallback_without_fixed_fps_tiers() {
        use crate::assessment::EncoderAssessment;
        use frame_contracts::{RecordingOptimization, RecordingPreset};
        let mut candidates = vec![
            candidate("h264", 4096, 2160, true),
            candidate("av1", 4096, 2160, true),
        ];
        candidates[1].codec = "av1".into();
        let mut inputs = PlanInputs {
            quality: Quality::High,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: rate(240, 1),
            candidates: &candidates,
        };
        let mut measurements = vec![
            (
                "h264".into(),
                EncoderAssessment {
                    completed_frames: 173,
                    elapsed_ns: 1_000_000_000,
                    failures: 0,
                    latency_p95_ns: Some(5_000_000),
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: Some(4_000_000),
                    dropped_frames: 0,
                },
            ),
            (
                "av1".into(),
                EncoderAssessment {
                    completed_frames: 1,
                    elapsed_ns: 1,
                    failures: 1,
                    latency_p95_ns: None,
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: None,
                    dropped_frames: 0,
                },
            ),
        ];
        let options = RecordingOptimization {
            preset: RecordingPreset::Gaming,
            ..Default::default()
        };
        let plan =
            plan_optimized_recording(&inputs, Some(&options), "h264", &measurements).unwrap();
        assert_eq!(plan.encoder.codec, "h264");
        assert_eq!(plan.nominal_rate, rate(173, 1));
        assert_eq!(plan.frame_rate, FrameRatePlan::Fixed(rate(173, 1)));
        measurements[0].1.dropped_frames = 17;
        let lost =
            plan_optimized_recording(&inputs, Some(&options), "h264", &measurements).unwrap();
        assert!(lost.nominal_rate.frames_per_second() < 173.0);
        inputs.nominal_rate = rate(960, 1);
        let unmeasured = plan_optimized_recording(&inputs, Some(&options), "h264", &[]).unwrap();
        assert_eq!(unmeasured.nominal_rate, rate(960, 1));
        assert_eq!(unmeasured.frame_rate, FrameRatePlan::SourcePaced);
        measurements[0].1.failures = 1;
        let fallback =
            plan_optimized_recording(&inputs, Some(&options), "h264", &measurements).unwrap();
        assert_eq!(fallback.encoder.codec, "h264");
        assert_eq!(fallback.nominal_rate, rate(960, 1));
    }

    #[test]
    fn optimizer_preserves_manual_selections_and_custom_bitrate() {
        use frame_contracts::{RecordingCodec, RecordingOptimization, RecordingPreset};
        let mut candidates = vec![candidate("explicit", 4096, 2160, true)];
        candidates[0].codec = "av1".into();
        let inputs = PlanInputs {
            quality: Quality::Maximum,
            resolution: Resolution::Native,
            fps_mode: FpsMode::Fixed(480.0),
            encoder: EncoderPreference::Explicit("explicit".into()),
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: rate(240, 1),
            candidates: &candidates,
        };
        let measurement = [(
            "explicit".into(),
            crate::assessment::EncoderAssessment {
                completed_frames: 30,
                elapsed_ns: 1_000_000_000,
                failures: 1,
                latency_p95_ns: Some(50_000_000),
                source_cost_p95_ns: None,
                recording_overhead_p95_ns: Some(30_000_000),
                dropped_frames: 10,
            },
        )];
        let legacy = plan_optimized_recording(&inputs, None, "av1", &measurement).unwrap();
        assert_eq!(legacy, plan_recording_for_codec(&inputs, "av1").unwrap());
        for preset in [
            RecordingPreset::Gaming,
            RecordingPreset::Desktop,
            RecordingPreset::Custom,
        ] {
            let options = RecordingOptimization {
                preset,
                codec: RecordingCodec::Av1,
                automatic_quality: false,
                bitrate_bps: Some(21_000_000),
                encoder_overrides: Default::default(),
            };
            let plan =
                plan_optimized_recording(&inputs, Some(&options), "h264", &measurement).unwrap();
            assert_eq!(plan.encoder.id, "explicit");
            assert_eq!(plan.nominal_rate, rate(480, 1));
            assert_eq!(plan.quality, Quality::Maximum);
            assert_eq!(plan.bitrate_bps, 21_000_000);
        }
    }

    #[test]
    fn lossless_is_native_transform_bypass_and_never_a_quality_alias() {
        use frame_contracts::{RecordingOptimization, RecordingPreset};
        let candidates = vec![candidate(
            "nvidia-nvenc-h264-d3d11:luid=1",
            4096,
            2160,
            true,
        )];
        let mut inputs = PlanInputs {
            quality: Quality::Standard,
            resolution: Resolution::Native,
            fps_mode: FpsMode::AutoMatchSource,
            encoder: EncoderPreference::Auto,
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: rate(144, 1),
            candidates: &candidates,
        };
        let options = RecordingOptimization {
            preset: RecordingPreset::Lossless,
            ..Default::default()
        };
        let plan = plan_optimized_recording(&inputs, Some(&options), "h264", &[]).unwrap();
        assert_eq!(plan.preset, EncoderPreset::Lossless);
        assert_eq!(plan.bitrate_bps, 0);
        let unavailable = vec![candidate("intel-mf-h264-d3d11:luid=1", 4096, 2160, false)];
        inputs.candidates = &unavailable;
        assert!(plan_optimized_recording(&inputs, Some(&options), "h264", &[]).is_err());
    }
}
