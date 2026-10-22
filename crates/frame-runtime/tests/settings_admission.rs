use frame_contracts::*;
use frame_runtime::recording_control::{EngineEventSink, RecordingControl};
use frame_settings::model::{AudioCodec, RecordingConfiguration};
use std::sync::Arc;

struct Sink;
impl EngineEventSink for Sink {
    fn publish(&self, _: EventEnvelope) {}
}
fn request() -> RecordingRequest {
    RecordingRequest {
        target: CaptureTarget::Display(DisplayId("must-not-open".into())),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: None,
        audio_routes: vec![AudioRouteSelection {
            controls: Default::default(),
            source_id: AudioSourceId("must-not-open".into()),
            route: AudioRoute::SeparateOnly,
            muted: false,
        }],
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            file_stem: "must-not-create".into(),
        },
    }
}

#[test]
fn every_controller_start_path_refuses_stems_only_before_capture() {
    let control = RecordingControl::new(Arc::new(Sink));
    for error in [
        control.start(request()).unwrap_err(),
        control.start_with_countdown(request(), 3).unwrap_err(),
    ] {
        assert!(error.message.contains("Main mix"));
    }
    #[cfg(windows)]
    assert!(
        control
            .start_replay(request())
            .unwrap_err()
            .message
            .contains("Main mix")
    );
    assert_eq!(control.published().state, RecorderState::Idle);
    assert!(control.published().last_artifact.is_none());
}

#[test]
fn whole_configuration_never_drops_unsupported_audio_at_start() {
    let control = RecordingControl::new(Arc::new(Sink));
    let mut c: RecordingConfiguration = request().into();
    c.request.audio_routes.clear();
    c.audio.codec = AudioCodec::Aac;
    assert!(
        control
            .start_configuration(&c)
            .unwrap_err()
            .message
            .contains("Opus")
    );
    assert_eq!(control.published().state, RecorderState::Idle);
}

#[test]
fn duplicate_capture_routes_are_refused_before_output_creation() {
    let control = RecordingControl::new(Arc::new(Sink));
    let mut r = request();
    r.audio_routes[0].route = AudioRoute::MainMixAndSeparate;
    r.audio_routes.push(r.audio_routes[0].clone());
    assert!(control.start(r).unwrap_err().message.contains("Duplicate"));
    assert_eq!(control.published().state, RecorderState::Idle);
}

#[test]
fn unsupported_native_video_intent_is_refused_by_all_start_entry_points() {
    let control = RecordingControl::new(Arc::new(Sink));
    for overrides in [
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Crf),
            ..Default::default()
        },
        VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Icq),
            ..Default::default()
        },
        VideoEncoderOverrides {
            b_frames: Some(2),
            ..Default::default()
        },
        VideoEncoderOverrides {
            lookahead_frames: Some(16),
            ..Default::default()
        },
    ] {
        let mut r = request();
        r.audio_routes.clear();
        r.optimization = Some(RecordingOptimization {
            encoder_overrides: overrides,
            ..Default::default()
        });
        for e in [
            control.start(r.clone()).unwrap_err(),
            control.start_with_countdown(r.clone(), 3).unwrap_err(),
            control.start_configuration(&r.clone().into()).unwrap_err(),
        ] {
            assert_eq!(e.code, ErrorCode::InvalidRequest);
        }
        #[cfg(windows)]
        assert_eq!(
            control.start_replay(r).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
    assert_eq!(control.published().state, RecorderState::Idle);
    assert!(control.published().last_artifact.is_none());
}

#[test]
fn invalid_professional_audio_with_native_video_is_refused_by_all_start_entry_points() {
    let control = RecordingControl::new(Arc::new(Sink));
    let mut r = request();
    r.audio_routes[0].route = AudioRoute::MainMixAndSeparate;
    r.audio_routes[0].controls.gain_millidb = 24001;
    r.optimization = Some(RecordingOptimization {
        encoder_overrides: VideoEncoderOverrides {
            rate_control: Some(VideoRateControl::Cqp),
            qp: Some(21),
            ..Default::default()
        },
        ..Default::default()
    });
    for e in [
        control.start(r.clone()).unwrap_err(),
        control.start_with_countdown(r.clone(), 3).unwrap_err(),
        control.start_configuration(&r.clone().into()).unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::InvalidRequest);
        assert!(e.message.contains("gain"));
    }
    #[cfg(windows)]
    assert_eq!(
        control.start_replay(r).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(control.published().state, RecorderState::Idle);
    assert!(control.published().last_artifact.is_none());
}
