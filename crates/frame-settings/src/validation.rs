//! Separate storage validity, current device admission, and live change policy.
use crate::{capabilities::*, model::*, policy::*};
use frame_contracts::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueCode {
    Invalid,
    Unsupported,
    SourceUnavailable,
    NeedsProbe,
    EngineBusy,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsIssue {
    pub field: &'static str,
    pub code: IssueCode,
    pub message: String,
}
#[derive(Clone, Debug, Default)]
pub struct ValidationReport {
    pub issues: Vec<SettingsIssue>,
    pub plan: Option<RecordingPlan>,
}
impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }
    fn issue(&mut self, field: &'static str, code: IssueCode, message: impl Into<String>) {
        self.issues.push(SettingsIssue {
            field,
            code,
            message: message.into(),
        });
    }
}

/// Offline load/save preserves preferences even if a device has been unplugged.
pub fn validate_document(document: &SettingsDocument) -> Result<(), String> {
    if document.schema_version != SCHEMA_VERSION {
        return Err("Unexpected settings schema version.".into());
    }
    fn check(c: &RecordingConfiguration) -> Result<(), String> {
        crate::validate_request(&c.request.for_capture()).map_err(|e| e.message)?;
        if c.audio.sample_rate_hz == 0 || c.audio.bitrate_bps == 0 {
            return Err("Audio rate and bitrate must be positive.".into());
        }
        if c.request.camera.as_ref().is_some_and(|camera| {
            camera.device_id.trim().is_empty()
                || camera.placement.width == 0
                || camera.placement.height == 0
        }) {
            return Err("Camera identity and placement must be nonempty.".into());
        }
        Ok(())
    }
    check(&document.defaults)?;
    for (id, profile) in &document.profiles {
        if id.trim().is_empty() || profile.name.trim().is_empty() {
            return Err("Profile ID and name must be nonempty.".into());
        }
        let mut c = document.defaults.clone();
        profile.overrides.apply(&mut c);
        check(&c)?;
    }
    check(&document.configuration()?)
}

pub fn validate_configuration(
    c: &RecordingConfiguration,
    caps: &SettingsCapabilities,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    let capture = c.request.for_capture();
    let r = &capture;
    if let Err(e) = crate::audio_admission::validate_audio_selection(
        &r.audio_routes,
        &caps.audio_sources,
        caps.engine.per_app_audio,
        caps.engine.microphone,
    ) {
        report.issue("audioRoutes", IssueCode::Unsupported, e.message);
    }
    if let Err(e) = crate::validate_request(r) {
        report.issue("request", IssueCode::Invalid, e.message);
    }
    if !std::path::Path::new(&r.output.directory).is_absolute() {
        report.issue(
            "output",
            IssueCode::Invalid,
            "Choose an absolute recording directory. Filesystem access is checked again at Start.",
        );
    }
    if !caps.engine.can_record {
        report.issue(
            "request",
            IssueCode::Unsupported,
            caps.engine
                .reason
                .clone()
                .unwrap_or_else(|| "Recording is unavailable.".into()),
        );
    }
    let supported_target = match &r.target {
        CaptureTarget::Display(id) => {
            if !caps.displays.iter().any(|d| d.id == *id) {
                report.issue(
                    "target",
                    IssueCode::SourceUnavailable,
                    "Display is unavailable.",
                );
            }
            caps.engine.display_capture
        }
        CaptureTarget::Region(region) => {
            match caps.displays.iter().find(|d| d.id == region.display_id) {
                Some(display) => {
                    if let Err(e) = crate::validate_region(region, display) {
                        report.issue("target", IssueCode::Invalid, e.message);
                    }
                }
                None => report.issue(
                    "target",
                    IssueCode::SourceUnavailable,
                    "Region display is unavailable.",
                ),
            }
            caps.engine.region_capture
        }
        CaptureTarget::Window(id) => {
            if !caps.windows.iter().any(|w| w.id == *id) {
                report.issue(
                    "target",
                    IssueCode::SourceUnavailable,
                    "Window is unavailable.",
                );
            }
            caps.engine.window_capture
        }
        CaptureTarget::Game(_) => caps.engine.game_capture,
    };
    if !supported_target {
        report.issue(
            "target",
            IssueCode::Unsupported,
            "This capture target is unsupported.",
        );
    }
    if matches!(r.fps_mode, FpsMode::Fixed(_)) && !caps.engine.fixed_fps {
        report.issue(
            "fpsMode",
            IssueCode::Unsupported,
            "Fixed frame rate is unavailable on this engine.",
        );
    }

    if r.camera.is_some()
        && (!caps.engine.camera_overlay
            || matches!(r.target, CaptureTarget::Window(_) | CaptureTarget::Game(_)))
    {
        report.issue(
            "camera",
            IssueCode::Unsupported,
            "Camera composition is unavailable for this capture path.",
        );
    }
    if let Some(camera) = &r.camera {
        let format = caps
            .cameras
            .iter()
            .find(|(device, _)| device.id == camera.device_id)
            .and_then(|(_, formats)| {
                formats
                    .iter()
                    .find(|f| f.index == camera.format_index && f.supported)
            });
        if format.is_none() {
            report.issue(
                "camera",
                IssueCode::SourceUnavailable,
                "Selected camera or native format is unavailable.",
            );
        }
        if let Some(video) = &caps.video {
            let p = camera.placement;
            if p.width == 0
                || p.height == 0
                || p.x
                    .checked_add(p.width)
                    .is_none_or(|v| v > video.native.width)
                || p.y
                    .checked_add(p.height)
                    .is_none_or(|v| v > video.native.height)
            {
                report.issue(
                    "camera",
                    IssueCode::Invalid,
                    "Camera placement must fit inside the recording canvas.",
                );
            }
        }
    }
    if let Err(error) = c.audio.validate() {
        report.issue("audio", IssueCode::Unsupported, error.message);
    }
    for route in &r.audio_routes {
        if route.route == AudioRoute::Ignore {
            continue;
        }
        if let Some((_, AudioDeviceQualification::Unavailable(reason))) = caps
            .audio_devices
            .iter()
            .find(|(id, _)| *id == route.source_id)
        {
            report.issue("audioRoutes", IssueCode::Unsupported, reason.clone());
        }
        match caps.audio_sources.iter().find(|s| s.id == route.source_id) {
            None => report.issue(
                "audioRoutes",
                IssueCode::SourceUnavailable,
                format!("Audio source {} is unavailable.", route.source_id.0),
            ),
            Some(source) => {
                if (source.kind == AudioSourceKind::Application && !caps.engine.per_app_audio)
                    || (source.kind == AudioSourceKind::Microphone && !caps.engine.microphone)
                    || (source.kind == AudioSourceKind::System
                        && (!crate::audio_admission::system_capture_supported(source)
                            || (source.id.0.starts_with("wasapi-rest:")
                                && !caps.engine.per_app_audio)))
                {
                    report.issue(
                        "audioRoutes",
                        IssueCode::Unsupported,
                        "This audio capture kind is not wired to the recorder.",
                    );
                }
            }
        }
    }
    match &caps.video {
        None => report.issue("video", IssueCode::NeedsProbe, "Video configuration has no native qualification. Refresh capabilities; portal geometry may require admission at Start."),
        Some(video) if video.key != VideoProbeKey::from(c) => report.issue("video", IssueCode::NeedsProbe, "Video settings changed since qualification. Refresh capabilities."),
        Some(video) => {
            if video.hdr_source && (matches!(r.target, CaptureTarget::Region(_)) || r.camera.is_some()) {
                report.issue("target", IssueCode::Unsupported, "HDR requires whole-display capture without overlays.");
            }
            let inputs = PlanInputs { quality: r.quality, resolution: r.resolution, fps_mode: r.fps_mode,
                encoder: r.encoder.clone(), native: video.native, nominal_rate: video.nominal_rate, candidates: &video.candidates };
            let default_codec = match video.default_codec.as_str() {
                "h264" => "h264", "hevc" => "hevc", "av1" => "av1",
                _ => { report.issue("video", IssueCode::Unsupported, "Unrecognized native codec."); return report; }
            };
            match plan_optimized_recording(&inputs, r.optimization.as_ref(), default_codec, &[]) {
                Err(e) => report.issue("video", IssueCode::Unsupported, e.message),
                Ok(plan) => {
                    if !plan.encoder_overrides.is_empty() && video.advanced_qualification.as_ref() != Some(&(plan.encoder.id.clone(), plan.encoder_overrides)) {
                        report.issue("optimization.encoderOverrides", IssueCode::NeedsProbe, "The complete native Advanced combination has not initialized on this encoder. Refresh hardware.");
                    }
                    if let Err(e) = validate_applied_plan(r, &plan) {
                        report.issue("video", IssueCode::Unsupported, e.message);
                    }
                    let gpu = video.capture_gpu_id.as_ref().and_then(|id| caps.gpus.iter().find(|g| &g.id == id));
                    if caps.os == OperatingSystem::Windows && gpu.is_some_and(|g| windows_adapter_required_quality(g.vendor_id) == Some(Quality::Standard))
                        && (plan.rate_control != RateControl::Cbr || plan.preset != EncoderPreset::LowLatency) {
                        report.issue("quality", IssueCode::Unsupported, "This Intel/AMD path currently requires Standard CBR with low latency.");
                    }
                    report.plan = Some(plan);
                }
            }
        }
    }
    report
}

/// All hosts enforce exact manual codec/encoder intent and refuse RC substitution.
pub fn validate_applied_plan(
    request: &RecordingRequest,
    plan: &RecordingPlan,
) -> Result<(), RecorderError> {
    if plan
        .decisions
        .iter()
        .any(|d| d.code == DecisionCode::RateControlCbrFallback)
    {
        return Err(RecorderError::invalid(
            "The selected quality requires VBR, but this encoder only supports CBR. Choose a supported quality or encoder.",
        ));
    }
    if let EncoderPreference::Explicit(id) = &request.encoder
        && plan.encoder.id != *id
    {
        return Err(RecorderError::invalid(
            "The recording plan substituted the selected encoder.",
        ));
    }
    if let Some(o) = &request.optimization {
        if plan.encoder_overrides != o.encoder_overrides {
            return Err(RecorderError::invalid(
                "The recording plan dropped native Advanced intent.",
            ));
        }
        let codec = match o.codec {
            RecordingCodec::Auto => None,
            RecordingCodec::H264 => Some("h264"),
            RecordingCodec::Hevc => Some("hevc"),
            RecordingCodec::Av1 => Some("av1"),
        };
        if codec.is_some_and(|c| c != plan.encoder.codec) {
            return Err(RecorderError::invalid(
                "The recording plan substituted the selected codec.",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyTiming {
    Now,
    NextRecording,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeEffect {
    PresentationOnly,
    Ready,
    NextRecording,
}
/// The settings layer never issues a live engine command. Mute has a dedicated
/// RecordingControl::update_mute API; track topology and encoder plans are frozen.
pub fn change_effect(
    old: &SettingsDocument,
    next: &SettingsDocument,
    state: &RecorderState,
    timing: ApplyTiming,
) -> Result<ChangeEffect, String> {
    if old.configuration()? == next.configuration()? {
        return Ok(ChangeEffect::PresentationOnly);
    }
    if matches!(state, RecorderState::Idle | RecorderState::Failed(_)) {
        return Ok(ChangeEffect::Ready);
    }
    match timing {
        ApplyTiming::NextRecording => Ok(ChangeEffect::NextRecording),
        ApplyTiming::Now => Err("Recording settings are frozen while preparing, recording, paused or finalizing. Apply for the next recording.".into()),
    }
}
