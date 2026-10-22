//! Adapts recording settings to native UI controls.
//! No settings schema, engine options, environment variables or vendor guesses are written here.
use frame_contracts::*;
use frame_settings::policy::{
    DecisionCode, EncoderCandidate, NominalRate, PlanInputs, RecordingPlan,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoContext {
    pub optimization: Option<RecordingOptimization>,
    pub encoder: EncoderPreference,
    pub quality: Quality,
    pub target: CaptureTarget,
    pub dimensions: Option<Dimensions>,
    pub nominal_fps: Option<f64>,
    pub fps_mode: FpsMode,
    pub hdr: bool,
    pub composed: bool,
    pub fixed_fps: bool,
    pub required_quality: Option<Quality>,
}
impl VideoContext {
    pub fn from_request(request: &RecordingRequest, snapshot: &EngineSnapshot) -> Option<Self> {
        let display = match &request.target {
            CaptureTarget::Display(id) => snapshot.displays.iter().find(|d| &d.id == id),
            CaptureTarget::Region(r) => snapshot.displays.iter().find(|d| d.id == r.display_id),
            _ => snapshot.displays.iter().find(|d| d.is_primary),
        }?;
        let dimensions = match &request.target {
            CaptureTarget::Display(_) => Some(Dimensions {
                width: display.width,
                height: display.height,
            }),
            CaptureTarget::Region(r) => Some(Dimensions {
                width: r.width,
                height: r.height,
            }),
            CaptureTarget::Window(id) => {
                snapshot.windows.iter().find(|w| &w.id == id)?;
                // WindowInfo intentionally carries no geometry. Obtain it on the background probe.
                None
            }
            _ => return None,
        };
        Some(Self {
            optimization: request.optimization.clone(),
            encoder: request.encoder.clone(),
            quality: request.quality,
            target: request.target.clone(),
            dimensions,
            nominal_fps: display.refresh_rate_hz,
            fps_mode: request.fps_mode,
            hdr: !matches!(request.target, CaptureTarget::Window(_)) && display.hdr_available,
            composed: matches!(request.target, CaptureTarget::Region(_))
                || request.camera.is_some(),
            fixed_fps: snapshot.capabilities.fixed_fps,
            required_quality: if matches!(request.target, CaptureTarget::Window(_)) {
                None
            } else {
                display.required_recording_quality
            },
        })
    }
    pub fn rate(&self) -> Result<NominalRate, String> {
        let fps = match self.fps_mode {
            FpsMode::Fixed(fps) => fps,
            FpsMode::AutoMatchSource => self
                .nominal_fps
                .ok_or("Source refresh rate is unavailable.")?,
        };
        NominalRate::recover_exact(fps).map_err(|e| e.message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncoderBackend {
    Nvenc,
    MediaFoundation,
}

#[derive(Clone, Debug)]
pub(crate) struct QualifiedEncoder {
    pub candidate: EncoderCandidate,
    pub label: String,
    pub backend: EncoderBackend,
    pub qualities: Vec<Quality>,
    pub lossless: bool,
    pub native_options: Vec<VideoEncoderOverrides>,
}

#[derive(Clone, Debug)]
pub(crate) struct VideoCapabilities {
    pub context: VideoContext,
    pub native_dimensions: Dimensions,
    pub adapter: String,
    pub encoders: Vec<QualifiedEncoder>,
}
impl VideoCapabilities {
    pub fn eligible<'a>(
        &'a self,
        request: &'a RecordingRequest,
    ) -> impl Iterator<Item = &'a QualifiedEncoder> {
        let codec = request
            .optimization
            .as_ref()
            .map_or(RecordingCodec::Auto, |o| o.codec);
        self.encoders.iter().filter(move |e| {
            (codec == RecordingCodec::Auto || codec_name(codec) == e.candidate.codec)
                && match &request.encoder {
                    EncoderPreference::Auto => true,
                    EncoderPreference::Explicit(id) => id == &e.candidate.id,
                }
        })
    }
    pub fn quality_supported(&self, request: &RecordingRequest, quality: Quality) -> bool {
        self.context.required_quality.is_none_or(|q| q == quality)
            && self
                .eligible(request)
                .any(|e| e.qualities.contains(&quality))
    }
    pub fn lossless_supported(&self, request: &RecordingRequest) -> bool {
        !self.context.hdr && self.eligible(request).any(|e| e.lossless)
    }
    pub fn plan(&self, request: &RecordingRequest) -> Result<RecordingPlan, String> {
        if matches!(self.context.target, CaptureTarget::Window(_)) && self.context.composed {
            return Err(
                "Window capture currently requires SDR native video without a camera overlay."
                    .into(),
            );
        }
        if self.context.hdr && self.context.composed {
            return Err("HDR capture requires a whole display without a camera overlay.".into());
        }
        let options = request.optimization.as_ref();
        let lossless = options.is_some_and(|o| o.preset == RecordingPreset::Lossless);
        let automatic =
            options.is_some_and(|o| o.automatic_quality && o.preset != RecordingPreset::Custom);
        let candidates = self
            .eligible(request)
            .filter(|e| {
                if lossless {
                    e.lossless
                } else if automatic {
                    !e.qualities.is_empty()
                } else {
                    e.qualities.contains(&request.quality)
                }
            })
            .map(|e| e.candidate.clone())
            .collect::<Vec<_>>();
        let default_codec = if self.context.hdr {
            "hevc"
        } else if matches!(&request.encoder, EncoderPreference::Explicit(id) if id.contains("av1"))
        {
            "av1"
        } else {
            "h264"
        };
        let inputs = PlanInputs {
            quality: request.quality,
            resolution: request.resolution,
            fps_mode: request.fps_mode,
            encoder: request.encoder.clone(),
            native: self.native_dimensions,
            nominal_rate: self.context.rate()?,
            candidates: &candidates,
        };
        let plan =
            frame_settings::policy::plan_optimized_recording(&inputs, options, default_codec, &[])
                .map_err(|e| e.message)?;
        let qualified = self
            .encoders
            .iter()
            .find(|e| e.candidate.id == plan.encoder.id)
            .ok_or("The planned encoder was not qualified for this source.")?;
        if !lossless
            && (!qualified.qualities.contains(&plan.quality)
                || self
                    .context
                    .required_quality
                    .is_some_and(|q| q != plan.quality))
        {
            return Err("The profile resolves to a quality that was not qualified for this source. Select a supported quality explicitly.".into());
        }
        if plan
            .decisions
            .iter()
            .any(|d| d.code == DecisionCode::RateControlCbrFallback)
        {
            return Err("This quality requests VBR, which this encoder did not qualify. Choose Standard CBR explicitly.".into());
        }
        Ok(plan)
    }
}

pub(crate) fn codec_name(codec: RecordingCodec) -> &'static str {
    match codec {
        RecordingCodec::Auto => "auto",
        RecordingCodec::H264 => "h264",
        RecordingCodec::Hevc => "hevc",
        RecordingCodec::Av1 => "av1",
    }
}

/// Clone, validate and return a complete request. Failure never partially mutates values.
pub(crate) fn apply_edit(
    request: &RecordingRequest,
    context: &VideoContext,
    caps: Option<&VideoCapabilities>,
    edit: &super::VideoEdit,
) -> Result<RecordingRequest, String> {
    use super::VideoEdit;
    let mut next = request.clone();
    let caps = caps.filter(|c| &c.context == context);
    match edit {
        VideoEdit::EncoderOverrides(value) if value.is_empty() => {
            if let Some(o) = &mut next.optimization {
                o.encoder_overrides = *value;
            }
        }
        VideoEdit::NativeResolution => next.resolution = Resolution::Native,
        VideoEdit::Fps(value) => {
            if value.is_some() && !context.fixed_fps {
                return Err("Fixed frame rate is unavailable for this capture path.".into());
            }
            if value == &Some(0) {
                return Err("Frame rate must be positive.".into());
            }
            next.fps_mode = value.map_or(FpsMode::AutoMatchSource, |n| {
                FpsMode::Fixed(f64::from(n) / 1000.)
            });
        }
        _ => {
            let caps =
                caps.ok_or("Check hardware for this source before changing encoder settings.")?;
            match edit {
                VideoEdit::EncoderOverrides(value) => {
                    if !value.is_empty()
                        && !caps
                            .eligible(request)
                            .any(|e| e.native_options.contains(value))
                    {
                        return Err("This exact Advanced combination has not initialized on the selected encoder.".into());
                    }
                    next.optimization
                        .get_or_insert_with(|| RecordingOptimization {
                            preset: RecordingPreset::Custom,
                            automatic_quality: false,
                            ..Default::default()
                        })
                        .encoder_overrides = *value;
                }
                VideoEdit::Quality(quality) => {
                    if let Some(quality) = quality {
                        if !caps.quality_supported(&next, *quality) {
                            return Err("This quality is not qualified for the selected encoder and source.".into());
                        }
                        next.quality = *quality;
                    }
                    let o = next
                        .optimization
                        .get_or_insert_with(|| RecordingOptimization {
                            preset: RecordingPreset::Custom,
                            automatic_quality: false,
                            ..Default::default()
                        });
                    o.automatic_quality = quality.is_none();
                    if quality.is_none() && o.preset == RecordingPreset::Custom {
                        o.preset = RecordingPreset::Desktop;
                    }
                }
                VideoEdit::Encoder(encoder) => next.encoder = encoder.clone(),
                VideoEdit::Codec(codec) => {
                    next.optimization
                        .get_or_insert_with(|| RecordingOptimization {
                            preset: RecordingPreset::Custom,
                            automatic_quality: false,
                            ..Default::default()
                        })
                        .codec = *codec
                }
                VideoEdit::Profile(profile) => {
                    if *profile == RecordingPreset::Lossless && !caps.lossless_supported(&next) {
                        return Err(
                            "Lossless QP 0 was not qualified for this encoder and source.".into(),
                        );
                    }
                    let o = next
                        .optimization
                        .get_or_insert_with(|| RecordingOptimization {
                            preset: RecordingPreset::Custom,
                            automatic_quality: false,
                            ..Default::default()
                        });
                    o.preset = *profile;
                    if *profile == RecordingPreset::Custom {
                        o.automatic_quality = false;
                    }
                }
                _ => unreachable!(),
            }
            caps.plan(&next)?;
        }
    }
    Ok(next)
}

/// Every menu value is qualified as a complete native configuration in the background.
#[derive(Clone, Copy)]
pub(crate) enum NativeControl {
    Rate,
    Qp,
    Preset,
    Tuning,
    Gop,
    Multipass,
    SpatialAq,
    TemporalAq,
}
impl NativeControl {
    pub const ALL: [Self; 8] = [
        Self::Rate,
        Self::Qp,
        Self::Preset,
        Self::Tuning,
        Self::Gop,
        Self::Multipass,
        Self::SpatialAq,
        Self::TemporalAq,
    ];
}
pub(crate) fn native_control_choices(
    current: VideoEncoderOverrides,
    control: NativeControl,
) -> Vec<(String, VideoEncoderOverrides)> {
    let mut result = Vec::new();
    macro_rules! choices {
        ($field:ident, $values:expr) => {
            for (label, value) in $values {
                let mut next = current;
                next.$field = value;
                result.push((label.to_string(), next));
            }
        };
    }
    match control {
        NativeControl::Rate => {
            for value in [
                None,
                Some(VideoRateControl::Cbr),
                Some(VideoRateControl::Vbr),
                Some(VideoRateControl::Cqp),
            ] {
                let mut next = current;
                next.rate_control = value;
                next.qp = if value == Some(VideoRateControl::Cqp) {
                    Some(current.qp.unwrap_or(23))
                } else {
                    None
                };
                result.push((
                    value.map_or("Automatic policy".into(), |v| format!("{v:?}")),
                    next,
                ));
            }
        }
        NativeControl::Qp => {
            for q in [0, 16, 18, 20, 23, 28, 32, 40, 51] {
                let mut next = current;
                next.qp = Some(q);
                result.push((format!("QP {q}"), next));
            }
        }
        NativeControl::Preset => {
            choices!(preset, [("Automatic policy", None)]);
            for p in [
                VideoEncoderPreset::P1,
                VideoEncoderPreset::P2,
                VideoEncoderPreset::P3,
                VideoEncoderPreset::P4,
                VideoEncoderPreset::P5,
                VideoEncoderPreset::P6,
                VideoEncoderPreset::P7,
            ] {
                let mut next = current;
                next.preset = Some(p);
                result.push((format!("{p:?}"), next));
            }
        }
        NativeControl::Tuning => choices!(
            tuning,
            [
                ("Automatic policy", None),
                (
                    "Ultra-low latency",
                    Some(VideoEncoderTuning::UltraLowLatency)
                ),
                ("Low latency", Some(VideoEncoderTuning::LowLatency)),
                ("High quality", Some(VideoEncoderTuning::HighQuality))
            ]
        ),
        NativeControl::Gop => {
            choices!(gop_frames, [("Automatic · two seconds nominal", None)]);
            for n in [30, 60, 120, 180, 360] {
                let mut next = current;
                next.gop_frames = Some(n);
                result.push((format!("{n} accepted frames"), next));
            }
        }
        NativeControl::Multipass => choices!(
            multipass,
            [
                ("Automatic policy", None),
                ("Off", Some(VideoMultipass::Disabled)),
                (
                    "Two passes · quarter resolution",
                    Some(VideoMultipass::QuarterResolution)
                ),
                (
                    "Two passes · full resolution",
                    Some(VideoMultipass::FullResolution)
                )
            ]
        ),
        NativeControl::SpatialAq => choices!(
            spatial_aq,
            [
                ("Automatic policy", None),
                ("Off", Some(false)),
                ("On", Some(true))
            ]
        ),
        NativeControl::TemporalAq => choices!(
            temporal_aq,
            [
                ("Automatic policy", None),
                ("Off", Some(false)),
                ("On", Some(true))
            ]
        ),
    }
    result
}

/// Probe on a background executor only. Does not acquire a capture frame or save settings.
#[cfg(target_os = "windows")]
pub(crate) fn discover(context: VideoContext) -> Result<VideoCapabilities, String> {
    use frame_settings::policy::{EncoderLimits, EncoderPreset, RateControl};
    use frame_windows::{
        NativeEncoderConfig, NativeEncoderPreset, NativeQualitySettings, NativeRateControl,
        NativeVideoProfile,
    };
    let mut dimensions = context.dimensions;
    let adapter = match &context.target {
        CaptureTarget::Display(id) => {
            if (frame_windows::active_hdr_display(id).map_err(|e| e.message)? == Some(true))
                != context.hdr
            {
                return Err(
                    "The display signal changed. Refresh sources and check hardware again.".into(),
                );
            }
            frame_windows::display_capture_adapter_identity(id)
        }
        CaptureTarget::Region(r) => frame_windows::display_capture_adapter_identity(&r.display_id),
        CaptureTarget::Window(id) => {
            let source = frame_windows::window_recording_source(id).map_err(|e| e.message)?;
            dimensions = Some(Dimensions {
                width: source.width,
                height: source.height,
            });
            Ok(source.adapter)
        }
        _ => return Err("Select a screen, area or window first.".into()),
    }
    .map_err(|e| e.message)?;
    if context.hdr && context.composed {
        return Err("HDR currently requires a whole display without a camera overlay.".into());
    }
    if matches!(context.target, CaptureTarget::Window(_)) && adapter.vendor_id != 0x10de {
        return Err(
            "The current window capture path requires NVENC on its capture adapter.".into(),
        );
    }
    let rate = context.rate()?;
    let dims = dimensions.ok_or("Capture geometry is unavailable.")?;
    let proof = NativeEncoderConfig::proof(dims.width, dims.height, rate.num(), rate.den())
        .map_err(|e| e.message)?;
    let profiles: &[NativeVideoProfile] = if context.hdr {
        &[
            NativeVideoProfile::HevcMain10,
            NativeVideoProfile::Av1Main10,
        ]
    } else {
        &[NativeVideoProfile::H264High, NativeVideoProfile::Av1Main8]
    };
    let mut probe_profiles = profiles
        .iter()
        .map(|&profile| (profile, false))
        .collect::<Vec<_>>();
    if adapter.vendor_id == 0x10de
        && !context.hdr
        && !matches!(context.target, CaptureTarget::Window(_))
    {
        // The runtime admits adapter-scoped NVIDIA MF AV1 as an explicit alternative.
        probe_profiles.push((NativeVideoProfile::Av1Main8, true));
    }
    let mut encoders = Vec::new();
    let mut failures = Vec::new();
    for (profile, force_mf) in probe_profiles {
        let qualified = if force_mf {
            frame_windows::probe_mf_av1_for_adapter(&adapter, &proof)
        } else if adapter.vendor_id == 0x10de {
            frame_windows::probe_native_profile_for_adapter(&adapter, &proof, profile)
        } else if profile == NativeVideoProfile::H264High {
            frame_windows::probe_h264_encoder_for_adapter(
                &adapter,
                (dims.width, dims.height),
                (rate.num(), rate.den()),
            )
            .and_then(|v| {
                v.into_iter()
                    .next()
                    .ok_or_else(|| RecorderError::invalid("No hardware H.264 encoder qualified."))
            })
        } else if !context.hdr {
            frame_windows::probe_mf_av1_for_adapter(&adapter, &proof)
        } else {
            continue;
        };
        let qualified = match qualified {
            Ok(c) => c,
            Err(e) => {
                failures.push(e.message);
                continue;
            }
        };
        let support = qualified.settings.rate_control_support();
        let candidate = EncoderCandidate {
            id: qualified.capability.id,
            codec: qualified.capability.codec,
            hardware: qualified.capability.hardware,
            limits: EncoderLimits {
                max_width: qualified.settings.caps_width_max,
                max_height: qualified.settings.caps_height_max,
                cbr: support.cbr,
                vbr: support.vbr,
            },
        };
        let nvenc = adapter.vendor_id == 0x10de && !force_mf;
        let mut qualities = Vec::new();
        let mut lossless = false;
        for quality in [Quality::Standard, Quality::High, Quality::Maximum] {
            if !nvenc && quality != Quality::Standard {
                continue;
            }
            if context.required_quality.is_some_and(|q| q != quality) {
                continue;
            }
            let inputs = PlanInputs {
                quality,
                resolution: Resolution::Native,
                fps_mode: context.fps_mode,
                encoder: EncoderPreference::Explicit(candidate.id.clone()),
                native: dims,
                nominal_rate: rate,
                candidates: std::slice::from_ref(&candidate),
            };
            let Ok(plan) =
                frame_settings::policy::plan_recording_for_codec(&inputs, profile.codec())
            else {
                continue;
            };
            if plan
                .decisions
                .iter()
                .any(|d| d.code == DecisionCode::RateControlCbrFallback)
            {
                continue;
            }
            let config = NativeEncoderConfig::with_quality(
                dims.width,
                dims.height,
                rate.num(),
                rate.den(),
                NativeQualitySettings {
                    bitrate_bps: plan.bitrate_bps,
                    peak_bitrate_bps: plan.peak_bitrate_bps,
                    gop_frames: plan.gop_frames,
                    rate_control: if plan.rate_control == RateControl::Vbr {
                        NativeRateControl::Vbr
                    } else {
                        NativeRateControl::Cbr
                    },
                    preset: match plan.preset {
                        EncoderPreset::LowLatency => NativeEncoderPreset::LowLatency,
                        EncoderPreset::HighQuality => NativeEncoderPreset::HighQuality,
                        EncoderPreset::MaximumQuality => NativeEncoderPreset::MaximumQuality,
                        EncoderPreset::Lossless => NativeEncoderPreset::Lossless,
                    },
                },
            )
            .map_err(|e| e.message)?;
            let accepted = if nvenc {
                frame_windows::probe_native_profile_for_adapter(&adapter, &config, profile).is_ok()
            } else if profile == NativeVideoProfile::Av1Main8 {
                frame_windows::probe_mf_av1_for_adapter(&adapter, &config).is_ok()
            } else {
                // MF H.264's public probe verifies Standard CBR hardware at the requested
                // geometry/rate. Exact bitrate/GOP are revalidated by recording admission.
                true
            };
            if accepted {
                qualities.push(quality);
            }
        }
        if nvenc && profile == NativeVideoProfile::H264High {
            let config = NativeEncoderConfig::with_quality(
                dims.width,
                dims.height,
                rate.num(),
                rate.den(),
                NativeQualitySettings {
                    bitrate_bps: 0,
                    peak_bitrate_bps: 0,
                    gop_frames: frame_settings::policy::keyframe_interval_frames(rate),
                    rate_control: NativeRateControl::Lossless,
                    preset: NativeEncoderPreset::Lossless,
                },
            )
            .map_err(|e| e.message)?;
            lossless =
                frame_windows::probe_native_profile_for_adapter(&adapter, &config, profile).is_ok();
        }
        let mut native_options = Vec::new();
        if nvenc && !context.hdr {
            let current = context
                .optimization
                .as_ref()
                .map(|o| o.encoder_overrides)
                .unwrap_or_default();
            let mut possibilities = vec![current];
            for control in NativeControl::ALL {
                possibilities.extend(
                    native_control_choices(current, control)
                        .into_iter()
                        .map(|(_, v)| v),
                );
            }
            possibilities.sort_by_key(|v| format!("{v:?}"));
            possibilities.dedup();
            for overrides in possibilities {
                let mut options =
                    context
                        .optimization
                        .clone()
                        .unwrap_or_else(|| RecordingOptimization {
                            preset: RecordingPreset::Custom,
                            automatic_quality: false,
                            ..Default::default()
                        });
                options.encoder_overrides = overrides;
                let inputs = PlanInputs {
                    quality: context.quality,
                    resolution: Resolution::Native,
                    fps_mode: context.fps_mode,
                    encoder: EncoderPreference::Explicit(candidate.id.clone()),
                    native: dims,
                    nominal_rate: rate,
                    candidates: std::slice::from_ref(&candidate),
                };
                let accepted = frame_settings::policy::plan_optimized_recording(
                    &inputs,
                    Some(&options),
                    profile.codec(),
                    &[],
                )
                .and_then(|plan| frame_runtime::encoder_calibration::config_for_plan(dims, &plan))
                .and_then(|config| {
                    frame_windows::probe_native_profile_for_adapter(&adapter, &config, profile)
                });
                if accepted.is_ok() {
                    native_options.push(overrides);
                }
            }
        }
        encoders.push(QualifiedEncoder {
            native_options,
            label: format!(
                "{} · {}",
                if nvenc {
                    "NVIDIA NVENC"
                } else {
                    "Hardware Media Foundation"
                },
                match profile {
                    NativeVideoProfile::H264High => "H.264",
                    NativeVideoProfile::HevcMain10 => "HEVC Main10",
                    NativeVideoProfile::Av1Main8 => "AV1",
                    NativeVideoProfile::Av1Main10 => "AV1 Main10",
                }
            ),
            backend: if nvenc {
                EncoderBackend::Nvenc
            } else {
                EncoderBackend::MediaFoundation
            },
            candidate,
            qualities,
            lossless,
        });
    }
    if encoders.is_empty() {
        return Err(failures
            .into_iter()
            .next()
            .unwrap_or_else(|| "No encoder qualified for this source.".into()));
    }
    Ok(VideoCapabilities {
        context,
        native_dimensions: dims,
        adapter: adapter.description,
        encoders,
    })
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn discover(_: VideoContext) -> Result<VideoCapabilities, String> {
    Err(
        "This Video adapter has no qualified encoder capability provider on this platform yet."
            .into(),
    )
}

#[cfg(test)]
pub(crate) mod tests;
