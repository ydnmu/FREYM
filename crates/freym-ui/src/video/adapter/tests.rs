use super::*;
use crate::video::{VideoEdit, VideoMode, VideoUiState};

#[test]
fn native_override_and_restore_preserve_legacy_manual_quality_intent() {
    let (mut request, mut caps) = fixture();
    request.optimization = None;
    caps.context.optimization = None;
    let overrides = VideoEncoderOverrides {
        rate_control: Some(VideoRateControl::Cbr),
        ..Default::default()
    };
    caps.encoders[0].native_options.push(overrides);
    let next = apply_edit(
        &request,
        &caps.context,
        Some(&caps),
        &VideoEdit::EncoderOverrides(overrides),
    )
    .unwrap();
    let o = next.optimization.as_ref().unwrap();
    assert!(!o.automatic_quality);
    assert_eq!(o.preset, RecordingPreset::Custom);
    assert_eq!(next.quality, request.quality);
    let reset = apply_edit(
        &request,
        &caps.context,
        None,
        &VideoEdit::EncoderOverrides(Default::default()),
    )
    .unwrap();
    assert_eq!(reset, request);
}

pub(crate) fn fixture() -> (RecordingRequest, VideoCapabilities) {
    let request = RecordingRequest {
        target: CaptureTarget::Display(DisplayId("test-display".into())),
        fps_mode: FpsMode::Fixed(60.),
        resolution: Resolution::Native,
        quality: Quality::High,
        encoder: EncoderPreference::Auto,
        optimization: Some(RecordingOptimization {
            preset: RecordingPreset::Custom,
            codec: RecordingCodec::H264,
            automatic_quality: false,
            bitrate_bps: Some(30_000_000),
            encoder_overrides: Default::default(),
        }),
        audio_encoding: Default::default(),
        audio_routes: vec![AudioRouteSelection {
            controls: Default::default(),
            source_id: AudioSourceId("preserved".into()),
            route: AudioRoute::MainMix,
            muted: true,
        }],
        camera: None,

        show_cursor: false,
        output: OutputSettings {
            directory: "untouched".into(),
            file_stem: "untouched".into(),
        },
    };
    let context = VideoContext {
        optimization: request.optimization.clone(),
        encoder: request.encoder.clone(),
        quality: request.quality,
        target: request.target.clone(),
        dimensions: Some(Dimensions {
            width: 1920,
            height: 1080,
        }),
        nominal_fps: Some(60.),
        fps_mode: request.fps_mode,
        hdr: false,
        composed: false,
        fixed_fps: true,
        required_quality: None,
    };
    let caps = VideoCapabilities {
        native_dimensions: context.dimensions.unwrap(),
        context,
        adapter: "Test adapter · fixture only".into(),
        encoders: vec![QualifiedEncoder {
            native_options: Vec::new(),
            candidate: EncoderCandidate {
                id: "nvidia-nvenc-h264-d3d11:luid=0000000000000001".into(),
                codec: "h264".into(),
                hardware: true,
                limits: frame_settings::policy::EncoderLimits {
                    max_width: 4096,
                    max_height: 4096,
                    cbr: true,
                    vbr: true,
                },
            },
            label: "NVENC fixture".into(),
            backend: EncoderBackend::Nvenc,
            qualities: vec![Quality::Standard, Quality::High, Quality::Maximum],
            lossless: true,
        }],
    };
    (request, caps)
}

#[test]
fn switching_modes_preserves_every_request_value_and_section_state() {
    let (request, caps) = fixture();
    let before = serde_json::to_vec(&request).unwrap();
    let mut state = VideoUiState {
        capabilities: Some(caps),
        expanded: [true, false, true, false, false, false, false],
        ..Default::default()
    };
    for _ in 0..10 {
        state.switch_mode(VideoMode::Advanced);
        state.switch_mode(VideoMode::Easy);
    }
    assert_eq!(serde_json::to_vec(&request).unwrap(), before);
    assert!(state.expanded[0] && state.expanded[2]);
}

#[test]
fn encoder_selection_maps_the_qualified_id_and_preserves_unrelated_values() {
    let (request, caps) = fixture();
    let id = caps.encoders[0].candidate.id.clone();
    let next = apply_edit(
        &request,
        &caps.context,
        Some(&caps),
        &VideoEdit::Encoder(EncoderPreference::Explicit(id.clone())),
    )
    .unwrap();
    assert_eq!(next.encoder, EncoderPreference::Explicit(id));
    assert_eq!(next.optimization, request.optimization);
    assert_eq!(next.audio_routes, request.audio_routes);
    assert_eq!(next.output, request.output);
    assert_eq!(next.fps_mode, request.fps_mode);
    assert_eq!(next.show_cursor, request.show_cursor);
}

#[test]
fn unknown_encoder_unsupported_codec_and_missing_provider_are_errors() {
    let (request, caps) = fixture();
    let before = request.clone();
    assert!(
        apply_edit(
            &request,
            &caps.context,
            Some(&caps),
            &VideoEdit::Encoder(EncoderPreference::Explicit("invented-qsv".into()))
        )
        .is_err()
    );
    assert!(
        apply_edit(
            &request,
            &caps.context,
            Some(&caps),
            &VideoEdit::Codec(RecordingCodec::Av1)
        )
        .is_err()
    );
    assert!(
        apply_edit(
            &request,
            &caps.context,
            None,
            &VideoEdit::Quality(Some(Quality::High))
        )
        .is_err()
    );
    assert_eq!(request, before);
}

#[test]
fn vbr_never_becomes_cbr_and_media_foundation_never_inherits_nvenc_quality() {
    let (mut request, mut caps) = fixture();
    caps.encoders[0].candidate.limits.vbr = false;
    assert!(caps.plan(&request).unwrap_err().contains("VBR"));
    caps.encoders[0].backend = EncoderBackend::MediaFoundation;
    caps.encoders[0].qualities = vec![Quality::Standard];
    caps.encoders[0].lossless = false;
    assert!(!caps.quality_supported(&request, Quality::High));
    request.quality = Quality::Standard;
    assert!(caps.plan(&request).is_ok());
    assert!(
        apply_edit(
            &request,
            &caps.context,
            Some(&caps),
            &VideoEdit::Quality(Some(Quality::Maximum))
        )
        .is_err()
    );
    assert!(
        apply_edit(
            &request,
            &caps.context,
            Some(&caps),
            &VideoEdit::Profile(RecordingPreset::Lossless)
        )
        .is_err()
    );
}

#[test]
fn saved_scaling_is_visible_until_an_explicit_native_edit_and_never_substituted() {
    let (mut request, caps) = fixture();
    request.resolution = Resolution::Exact(Dimensions {
        width: 1280,
        height: 720,
    });
    assert!(caps.plan(&request).is_err());
    let next = apply_edit(
        &request,
        &caps.context,
        Some(&caps),
        &VideoEdit::NativeResolution,
    )
    .unwrap();
    assert_eq!(next.resolution, Resolution::Native);
    assert_eq!(
        request.resolution,
        Resolution::Exact(Dimensions {
            width: 1280,
            height: 720
        })
    );
}

#[test]
fn stale_probe_completion_and_mismatched_geometry_cannot_enable_controls() {
    let (request, caps) = fixture();
    let mut state = VideoUiState::default();
    let first = state.begin_probe(caps.context.clone());
    let mut changed = caps.context.clone();
    changed.dimensions.as_mut().unwrap().width = 2560;
    let second = state.begin_probe(changed.clone());
    state.finish_probe(first, Ok(caps.clone()));
    assert!(state.capabilities.is_none());
    assert_eq!(state.probing, Some(changed.clone()));
    assert!(
        apply_edit(
            &request,
            &changed,
            Some(&caps),
            &VideoEdit::Quality(Some(Quality::Standard))
        )
        .is_err()
    );
    state.finish_probe(second, Err("device removed".into()));
    assert!(state.probing.is_none());
    assert_eq!(state.probe_error.as_deref(), Some("device removed"));
}

#[test]
fn fps_capability_and_fractional_rates_do_not_depend_on_encoder_probe() {
    let (request, caps) = fixture();
    let next = apply_edit(&request, &caps.context, None, &VideoEdit::Fps(Some(59_940))).unwrap();
    assert_eq!(next.fps_mode, FpsMode::Fixed(59.94));
    let mut context = caps.context.clone();
    context.fixed_fps = false;
    assert!(apply_edit(&request, &context, None, &VideoEdit::Fps(Some(60_000))).is_err());
    assert_eq!(
        apply_edit(&request, &context, None, &VideoEdit::Fps(None))
            .unwrap()
            .fps_mode,
        FpsMode::AutoMatchSource
    );
    assert!(apply_edit(&request, &caps.context, None, &VideoEdit::Fps(Some(0))).is_err());
}

#[test]
fn hdr_composition_and_lossless_bitrate_conflicts_are_explicit() {
    let (request, mut caps) = fixture();
    assert!(
        apply_edit(
            &request,
            &caps.context,
            Some(&caps),
            &VideoEdit::Profile(RecordingPreset::Lossless)
        )
        .unwrap_err()
        .contains("bitrate")
    );
    caps.context.hdr = true;
    caps.context.composed = true;
    assert!(caps.plan(&request).unwrap_err().contains("whole display"));
    caps.context.hdr = false;
    caps.context.target = CaptureTarget::Window(WindowId("fixture".into()));
    assert!(
        caps.plan(&request)
            .unwrap_err()
            .contains("without a camera")
    );
}

#[test]
fn bitrate_over_codec_ceiling_fails_without_truncation() {
    let (mut request, caps) = fixture();
    request.optimization.as_mut().unwrap().bitrate_bps = Some(u32::MAX);
    assert!(caps.plan(&request).unwrap_err().contains("limit"));
    assert_eq!(request.optimization.unwrap().bitrate_bps, Some(u32::MAX));
}
