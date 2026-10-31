//! Native settings admission for GPUI and other hosts. All discovery is blocking;
//! call on a background executor while idle, then discard stale probe keys.
use frame_contracts::*;
use frame_settings::{capabilities::*, model::RecordingConfiguration};

/// Uses actual native discovery and adapter-scoped qualification, never a vendor list.
/// Encoder probes are prohibited during recording transitions and active sessions.
pub fn discover(
    configuration: &RecordingConfiguration,
    state: &RecorderState,
) -> Result<SettingsCapabilities, RecorderError> {
    if !matches!(state, RecorderState::Idle | RecorderState::Failed(_)) {
        return Err(RecorderError::invalid(
            "Refresh encoder capabilities only when recording is idle.",
        ));
    }
    frame_settings::validate_request(&configuration.request)?;
    let mut caps = SettingsCapabilities::from_snapshot(crate::discovery::snapshot()?);
    #[cfg(target_os = "windows")]
    {
        match frame_windows::windows_build_number() {
            Ok(build) => {
                caps.os_build = Some(build);
                caps.engine.per_app_audio &= build >= 20348;
            }
            Err(e) => {
                caps.engine.per_app_audio = false;
                caps.diagnostics.push(e.message);
            }
        }
        for source in &caps.audio_sources {
            let qualification = if source.kind == AudioSourceKind::Microphone {
                match frame_windows::probe_microphone_format(&source.id) {
                    Ok(format) => AudioDeviceQualification::Format {
                        sample_rate_hz: format.sample_rate_hz,
                        channels: format.channels,
                        sample_encoding: format.sample_encoding,
                    },
                    Err(e) => AudioDeviceQualification::Unavailable(e.message),
                }
            } else {
                AudioDeviceQualification::AtStart
            };
            caps.audio_devices.push((source.id.clone(), qualification));
        }
        caps.gpus = frame_windows::enumerate_native_adapters()?
            .into_iter()
            .map(|g| GpuCapability {
                id: format!("luid={:016X}", g.luid),
                name: g.description,
                vendor_id: g.vendor_id,
                driver_version: frame_windows::adapter_driver_version(g.luid).ok(),
            })
            .collect();
        // Format discovery opens the camera device. Never do this for unrelated settings.
        if let Some(selected) = configuration.request.camera.as_ref() {
            match frame_windows::camera::enumerate_cameras() {
                Ok(cameras) => {
                    if let Some(camera) = cameras.into_iter().find(|c| c.id == selected.device_id) {
                        match frame_windows::camera::enumerate_camera_formats(&camera.id) {
                            Ok(formats) => caps.cameras.push((camera, formats)),
                            Err(e) => caps
                                .diagnostics
                                .push(format!("Camera {}: {}", camera.name, e.message)),
                        }
                    }
                }
                Err(e) => caps
                    .diagnostics
                    .push(format!("Camera discovery: {}", e.message)),
            }
        }
        match windows_video(configuration, &caps) {
            Ok(video) => caps.video = Some(video),
            Err(e) => caps
                .diagnostics
                .push(format!("Video qualification: {}", e.message)),
        }
    }
    #[cfg(target_os = "macos")]
    match macos_video(configuration, &caps) {
        Ok(video) => caps.video = Some(video),
        Err(e) => caps
            .diagnostics
            .push(format!("Video qualification: {}", e.message)),
    }
    #[cfg(target_os = "linux")]
    {
        caps.diagnostics.push("Portal geometry and GPU identity require the selected PipeWire stream; exact video admission occurs at Start. No settings-only qualification is claimed.".into());
    }
    caps.diagnostics.push("Audio output is fixed to Opus, 48 kHz, 128 kb/s per track; mono/stereo layout follows the native source/track plan. Hardware discovery does not grant microphone or capture permission.".into());
    Ok(caps)
}

#[cfg(target_os = "windows")]
fn windows_video(
    configuration: &RecordingConfiguration,
    caps: &SettingsCapabilities,
) -> Result<VideoCapabilities, RecorderError> {
    use frame_settings::{NominalRate, validate_region};
    let r = &configuration.request;
    let window_id = match &r.target {
        CaptureTarget::Window(id) => Some(id.clone()),
        CaptureTarget::Game(game) => Some(frame_windows::resolve_game_window(game)?),
        _ => None,
    };
    let window = window_id
        .as_ref()
        .map(frame_windows::window_recording_source)
        .transpose()?;
    let display = match &r.target {
        CaptureTarget::Display(id) => caps.displays.iter().find(|d| d.id == *id),
        CaptureTarget::Region(region) => caps.displays.iter().find(|d| d.id == region.display_id),
        _ => caps.displays.iter().find(|d| d.is_primary),
    }
    .ok_or_else(|| RecorderError::invalid("Selected capture display is unavailable."))?;
    let hdr = window.is_none() && frame_windows::active_hdr_display(&display.id)? == Some(true);
    crate::video_recording::validate_encoder_signal(
        hdr,
        match &r.encoder {
            EncoderPreference::Explicit(id) => Some(id),
            _ => None,
        },
    )?;
    let (native, adapter) = if let Some(window) = window {
        if window.adapter.vendor_id != 0x10DE {
            return Err(RecorderError::invalid(
                "The isolated WGC path requires NVENC on its capture adapter.",
            ));
        }
        (
            Dimensions {
                width: window.width,
                height: window.height,
            },
            window.adapter,
        )
    } else {
        let native = match &r.target {
            CaptureTarget::Region(region) => {
                let bounds = validate_region(region, display)?;
                Dimensions {
                    width: bounds.width(),
                    height: bounds.height(),
                }
            }
            _ => Dimensions {
                width: display.width,
                height: display.height,
            },
        };
        (
            native,
            frame_windows::display_capture_adapter_identity(&display.id)?,
        )
    };
    let rate = if window_id.is_some() {
        match r.fps_mode {
            FpsMode::Fixed(fps) => NominalRate::recover_exact(fps)?,
            _ => source_rate(display)?,
        }
    } else {
        source_rate(display)?
    };
    // These are the same candidate probes used by recording preparation. Optional
    // codec failures simply leave the corresponding encoder absent.
    let candidates = crate::recording_control::windows_policy_candidates(
        r,
        &adapter,
        native,
        rate.num(),
        rate.den(),
        hdr,
    )?;
    let mut advanced_qualification = None;
    if r.optimization
        .as_ref()
        .is_some_and(|o| !o.encoder_overrides.is_empty())
    {
        let inputs = frame_settings::PlanInputs {
            quality: r.quality,
            resolution: r.resolution,
            fps_mode: r.fps_mode,
            encoder: r.encoder.clone(),
            native,
            nominal_rate: rate,
            candidates: &candidates,
        };
        let default_codec = if hdr {
            "hevc"
        } else if matches!(&r.encoder, EncoderPreference::Explicit(id) if id.contains("av1")) {
            "av1"
        } else {
            "h264"
        };
        let plan = frame_settings::policy::plan_optimized_recording(
            &inputs,
            r.optimization.as_ref(),
            default_codec,
            &[],
        )?;
        let config = crate::encoder_calibration::config_for_plan(native, &plan)?;
        let profile = if plan.encoder.codec == "av1" {
            frame_windows::NativeVideoProfile::Av1Main8
        } else {
            frame_windows::NativeVideoProfile::H264High
        };
        frame_windows::probe_native_profile_for_adapter(&adapter, &config, profile)?;
        advanced_qualification = Some((plan.encoder.id, plan.encoder_overrides));
    }
    Ok(VideoCapabilities {
        advanced_qualification,
        key: configuration.into(),
        capture_gpu_id: Some(format!("luid={:016X}", adapter.luid)),
        native,
        nominal_rate: rate,
        hdr_source: hdr,
        default_codec:
            if matches!(&r.encoder, EncoderPreference::Explicit(id) if id.contains("av1")) {
                "av1"
            } else if hdr {
                "hevc"
            } else {
                "h264"
            }
            .into(),
        candidates,
    })
}

#[cfg(target_os = "windows")]
fn source_rate(display: &DisplayInfo) -> Result<frame_settings::NominalRate, RecorderError> {
    frame_settings::NominalRate::recover_exact(
        display
            .refresh_rate_hz
            .ok_or_else(|| RecorderError::invalid("Source refresh rate is unknown."))?,
    )
}

#[cfg(target_os = "macos")]
fn macos_video(
    configuration: &RecordingConfiguration,
    caps: &SettingsCapabilities,
) -> Result<VideoCapabilities, RecorderError> {
    use frame_settings::{EncoderCandidate, EncoderLimits, NominalRate};
    let r = &configuration.request;
    let display = match &r.target {
        CaptureTarget::Display(id) => caps.displays.iter().find(|d| d.id == *id),
        CaptureTarget::Window(_) => caps.displays.iter().find(|d| d.is_primary),
        _ => None,
    }
    .ok_or_else(|| RecorderError::invalid("Selected macOS capture target is unavailable."))?;
    let (width, height) = frame_macos::capture_dimensions(&r.target)?;
    let rate = NominalRate::recover_exact(
        display
            .refresh_rate_hz
            .filter(|r| r.is_finite() && *r > 0.)
            .unwrap_or(60.),
    )?;
    let probe = frame_macos::probe_encoder(frame_macos::MacEncoderConfig {
        width,
        height,
        rate_num: rate.num(),
        rate_den: rate.den(),
        bitrate_bps: 1_000_000,
        peak_bitrate_bps: 2_000_000,
        gop_frames: 60,
    })?;
    Ok(VideoCapabilities {
        advanced_qualification: None,
        key: configuration.into(),
        capture_gpu_id: None,
        native: Dimensions { width, height },
        nominal_rate: rate,
        hdr_source: false,
        default_codec: "h264".into(),
        candidates: vec![EncoderCandidate {
            id: probe.id,
            codec: probe.codec,
            hardware: probe.hardware,
            limits: EncoderLimits {
                max_width: width,
                max_height: height,
                cbr: false,
                vbr: true,
            },
        }],
    })
}

impl crate::recording_control::RecordingControl {
    /// Converts the whole configuration at admission; unsupported fields cannot
    /// disappear when entering the existing native RecordingRequest API.
    pub fn start_configuration(
        &self,
        configuration: &RecordingConfiguration,
    ) -> Result<(), RecorderError> {
        self.start(configuration.engine_request()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_busy_discovery_before_touching_devices() {
        let c: RecordingConfiguration = RecordingRequest {
            target: CaptureTarget::Display(DisplayId("never-open-this".into())),
            fps_mode: FpsMode::AutoMatchSource,
            resolution: Resolution::Native,
            quality: Quality::Standard,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_encoding: Default::default(),
            audio_routes: vec![],
            camera: None,

            show_cursor: true,
            output: OutputSettings {
                directory: "recordings".into(),
                file_stem: "test".into(),
            },
        }
        .into();
        for state in [
            RecorderState::Preparing,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Finalizing,
        ] {
            assert!(discover(&c, &state).unwrap_err().message.contains("idle"));
        }
    }
}
