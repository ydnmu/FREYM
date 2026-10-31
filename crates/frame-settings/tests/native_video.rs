use frame_contracts::*;
use frame_settings::{model::*, policy::plan_optimized_recording, *};

fn plan(
    overrides: VideoEncoderOverrides,
    bitrate: Option<u32>,
    quality: Quality,
    fps: u32,
) -> Result<RecordingPlan, RecorderError> {
    let candidates = [EncoderCandidate {
        id: "nvidia-nvenc-h264-d3d11:luid=0000000000000001".into(),
        codec: "h264".into(),
        hardware: true,
        limits: EncoderLimits {
            max_width: 8192,
            max_height: 8192,
            cbr: true,
            vbr: true,
        },
    }];
    plan_optimized_recording(
        &PlanInputs {
            quality,
            resolution: Resolution::Native,
            fps_mode: FpsMode::Fixed(fps.into()),
            encoder: EncoderPreference::Auto,
            native: Dimensions {
                width: 1920,
                height: 1080,
            },
            nominal_rate: NominalRate::recover_exact(fps.into()).unwrap(),
            candidates: &candidates,
        },
        Some(&RecordingOptimization {
            automatic_quality: false,
            encoder_overrides: overrides,
            bitrate_bps: bitrate,
            ..Default::default()
        }),
        "h264",
        &[],
    )
}

#[test]
fn independent_rate_control_retains_quality_and_has_no_fallback() {
    for (rc, expected, quality) in [
        (VideoRateControl::Cbr, RateControl::Cbr, Quality::Maximum),
        (VideoRateControl::Vbr, RateControl::Vbr, Quality::Standard),
    ] {
        let p = plan(
            VideoEncoderOverrides {
                rate_control: Some(rc),
                ..Default::default()
            },
            Some(12_000_000),
            quality,
            60,
        )
        .unwrap();
        assert_eq!(p.rate_control, expected);
        assert_eq!(p.quality, quality);
        assert_eq!(p.bitrate_bps, 12_000_000);
        assert!(
            !p.decisions
                .iter()
                .any(|d| d.code == DecisionCode::RateControlCbrFallback)
        );
    }
    let p = plan(
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            qp: Some(23),
            ..Default::default()
        },
        None,
        Quality::High,
        60,
    )
    .unwrap();
    assert_eq!(
        (p.rate_control, p.bitrate_bps, p.peak_bitrate_bps),
        (RateControl::Cqp, 0, 0)
    );
}

#[test]
fn incompatible_modes_and_buffering_are_rejected_without_normalization() {
    for bad in [
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Icq),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Crf),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            ..Default::default()
        },
        VideoEncoderOverrides {
            qp: Some(23),
            ..Default::default()
        },
        VideoEncoderOverrides {
            b_frames: Some(1),
            ..Default::default()
        },
        VideoEncoderOverrides {
            lookahead_frames: Some(1),
            ..Default::default()
        },
        VideoEncoderOverrides {
            gop_frames: Some(0),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            qp: Some(52),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            qp: Some(23),
            spatial_aq: Some(true),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            qp: Some(23),
            multipass: Some(VideoMultipass::FullResolution),
            ..Default::default()
        },
    ] {
        assert!(plan(bad, None, Quality::Standard, 60).is_err(), "{bad:?}");
    }
    assert!(
        plan(
            VideoEncoderOverrides {
                rate_control: Some(VideoRateControl::Cqp),
                qp: Some(23),
                ..Default::default()
            },
            Some(1_000_000),
            Quality::Standard,
            60
        )
        .is_err()
    );
}

#[test]
fn automatic_defaults_retain_throughput_policy_and_manual_intent_is_frozen() {
    let p = plan(Default::default(), None, Quality::High, 180).unwrap();
    assert_eq!(p.preset, EncoderPreset::LowLatency);
    assert_eq!(p.rate_control, RateControl::Vbr);
    assert_eq!(p.gop_frames, 360);
    assert!(p.encoder_overrides.is_empty());
    let o = VideoEncoderOverrides {
        preset: Some(VideoEncoderPreset::P7),
        tuning: Some(VideoEncoderTuning::HighQuality),
        gop_frames: Some(42),
        ..Default::default()
    };
    let p = plan(o, None, Quality::High, 180).unwrap();
    assert_eq!(p.encoder_overrides, o);
    assert_eq!(p.gop_frames, 42);
}

#[test]
fn older_optimization_loads_defaults_and_unknown_advanced_fields_fail() {
    let old: RecordingOptimization = serde_json::from_str(
        r#"{"preset":"desktop","codec":"auto","automaticQuality":true,"bitrateBps":null}"#,
    )
    .unwrap();
    assert_eq!(old, RecordingOptimization::default());
    assert!(serde_json::from_str::<VideoEncoderOverrides>(r#"{"pretendQuality":42}"#).is_err());
}

#[test]
fn advanced_roundtrip_profile_and_mode_switch_preserve_exact_intent() {
    let request = RecordingRequest {
        target: CaptureTarget::Display(DisplayId("test".into())),
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
            directory: "C:/recordings".into(),
            file_stem: "test".into(),
        },
    };
    let mut doc = SettingsDocument::new(request);
    let mut c = doc.configuration().unwrap();
    let exact = VideoEncoderOverrides {
        rate_control: Some(VideoRateControl::Cqp),
        qp: Some(17),
        preset: Some(VideoEncoderPreset::P4),
        gop_frames: Some(73),
        ..Default::default()
    };
    c.request.optimization = Some(RecordingOptimization {
        encoder_overrides: exact,
        ..Default::default()
    });
    doc.set_configuration(&c).unwrap();
    doc.capture_profile("exact".into(), "Exact".into()).unwrap();
    doc.select_profile(Some("exact")).unwrap();
    doc.modes.video = SettingsMode::Easy;
    let loaded: SettingsDocument =
        serde_json::from_slice(&serde_json::to_vec(&doc).unwrap()).unwrap();
    assert_eq!(loaded, doc);
    assert_eq!(
        loaded
            .configuration()
            .unwrap()
            .request
            .optimization
            .unwrap()
            .encoder_overrides,
        exact
    );
    assert_eq!(
        video_easy_summary(&loaded.configuration().unwrap()),
        RecordingPreset::Custom
    );
}
