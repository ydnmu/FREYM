//! Capture-free native Video/Audio configuration smoke check in a new evidence directory.
use frame_contracts::*;
use frame_settings::{SettingsStore, model::*, validation::*};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--frame-mf-helper") {
        std::process::exit(frame_windows::run_mf_helper());
    }
    let directory = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("New evidence directory required")?,
    );
    if !directory.is_absolute() || directory.exists() {
        return Err("Use a new absolute evidence directory".into());
    }
    std::fs::create_dir(&directory)?;
    let snapshot = frame_runtime::discovery::snapshot()?;
    let display = snapshot
        .displays
        .iter()
        .find(|d| d.is_primary)
        .ok_or("No primary display")?;
    let microphone = snapshot
        .audio_sources
        .iter()
        .find(|s| s.kind == AudioSourceKind::Microphone && s.is_active)
        .ok_or("No active microphone")?;
    let desktop = snapshot
        .audio_sources
        .iter()
        .find(|s| s.id.0.starts_with("wasapi-loopback:") && s.is_active)
        .ok_or("No active desktop endpoint")?;
    let routes = [microphone, desktop]
        .into_iter()
        .map(|s| AudioRouteSelection {
            source_id: s.id.clone(),
            route: AudioRoute::MainMixAndSeparate,
            muted: false,
            controls: AudioSourceControls {
                gain_millidb: -3000,
                balance: 20,
                reconnect: AudioReconnectPolicy::ExactDevice,
            },
        })
        .collect();
    let c: RecordingConfiguration = RecordingRequest {
        target: CaptureTarget::Display(display.id.clone()),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization {
            automatic_quality: false,
            codec: RecordingCodec::H264,
            encoder_overrides: VideoEncoderOverrides {
                rate_control: Some(VideoRateControl::Cqp),
                qp: Some(21),
                preset: Some(VideoEncoderPreset::P2),
                tuning: Some(VideoEncoderTuning::UltraLowLatency),
                gop_frames: Some(60),
                multipass: Some(VideoMultipass::Disabled),
                spatial_aq: Some(false),
                temporal_aq: Some(false),
                b_frames: Some(0),
                lookahead_frames: Some(0),
            },
            ..Default::default()
        }),
        audio_routes: routes,
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: directory.to_string_lossy().into_owned(),
            file_stem: "Configuration_smoke".into(),
        },
    }
    .into();
    let caps = frame_runtime::settings::discover(&c, &RecorderState::Idle)?;
    let report = validate_configuration(&c, &caps);
    if !report.is_valid() {
        return Err(format!("Native configuration refused: {:?}", report.issues).into());
    }
    let plan = report.plan.ok_or("No validated video plan")?;
    assert_eq!(
        plan.encoder_overrides,
        c.request.optimization.as_ref().unwrap().encoder_overrides
    );
    let store = SettingsStore::in_directory(&directory);
    let mut document = SettingsDocument::new(c.clone());
    document.modes.video = SettingsMode::Advanced;
    document.modes.audio = SettingsMode::Easy;
    document.capture_profile("combined".into(), "Combined".into())?;
    document.select_profile(Some("combined"))?;
    let (saved, _) = store.commit(&document, 0, &caps, &RecorderState::Idle, ApplyTiming::Now)?;
    assert_eq!(saved.schema_version, 4);
    assert_eq!(store.load_document()?.unwrap(), saved);
    assert_eq!(saved.configuration()?, c);
    assert_eq!(c.engine_request()?, c.request);
    let mut modes = saved.clone();
    modes.modes.audio = SettingsMode::Advanced;
    modes.modes.video = SettingsMode::Easy;
    let (modes, _) = store.commit(&modes, 1, &caps, &RecorderState::Idle, ApplyTiming::Now)?;
    assert_eq!(modes.configuration()?, c);
    let restored = store.rollback(2, &RecorderState::Idle, ApplyTiming::Now)?;
    assert_eq!(restored.configuration()?, c);
    assert_eq!(restored.modes, saved.modes);
    let mut unavailable = c.clone();
    unavailable.request.audio_routes[0].source_id =
        AudioSourceId("wasapi-mic:missing-smoke-device".into());
    assert!(!validate_configuration(&unavailable, &caps).is_valid());
    let mut invalid = c.clone();
    invalid
        .request
        .optimization
        .as_mut()
        .unwrap()
        .encoder_overrides
        .b_frames = Some(2);
    assert!(invalid.engine_request().is_err());
    std::fs::write(
        directory.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": saved.schema_version,
            "encoder": plan.encoder.id,
                "rateControl": format!("{:?}", plan.rate_control),
            "nativeOverrides": plan.encoder_overrides,
            "audioRoutes": c.request.audio_routes,
            "revisions": [saved.revision, modes.revision, restored.revision],
            "profileReload": true,
            "independentModes": true,
            "rollback": true,
            "missingDeviceRefused": true,
            "unsupportedVideoRefused": true,
            "captureStarted": false
        }))?,
    )?;
    println!(
        "PASS: schema 4, exact native CQP/P2/ULL/GOP60, microphone + desktop stems, gain/balance/reconnect, profile, independent modes, reload and rollback. No capture started."
    );
    Ok(())
}
