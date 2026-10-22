use super::*;
use gpui::{Modifiers, TestAppContext, size};

fn source(id: &str, route: AudioRoute) -> AudioSource {
    AudioSource {
        controls: Default::default(),
        capture_identity: None,
        id: AudioSourceId(id.into()),
        kind: AudioSourceKind::Microphone,
        display_name: id.into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route,
        muted: false,
    }
}

fn setup(home: &mut Home, sources: Vec<AudioSource>) {
    home.model.snapshot.capabilities.per_app_audio = true;
    home.services
        .control
        .restore_routes(&routing::selections(&sources));
    home.model
        .apply_published(home.services.control.observe_sources(sources));
    home.model.request = Some(RecordingRequest {
        target: CaptureTarget::Display(DisplayId("display".into())),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: None,
        audio_routes: home.model.selected_routes(),
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: ".".into(),
            file_stem: "test".into(),
        },
    });
    home.model.sheet = Some(Sheet::Audio);
}

// Deliver the real command worker's snapshots through production settlement.
// Fixtures skip only filesystem/discovery startup and never acquire a device.
fn settle(home: &mut Home, cx: &mut Context<Home>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while home.model.pending_route.is_some()
        || !home.audio_ui.routes.is_empty()
        || home.audio_ui.processing.is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "route acknowledgement timed out"
        );
        home.model
            .apply_published(home.services.control.published());
        home.model.sync_routes();
        home.advance_audio_routes(cx);
        std::thread::yield_now();
    }
}

#[gpui::test]
fn audio_mode_switch_preserves_routes_video_settings_and_expansion(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    view.update(cx, |home, cx| {
        setup(home, vec![source("mic", AudioRoute::MainMix)]);
        home.model.preferences.extra_preferences.insert(
            "audio_settings".into(),
            serde_json::json!({"future_audio":42}),
        );
        cx.notify();
    });
    cx.run_until_parked();
    let before = cx.update(|_, cx| {
        let h = view.read(cx);
        (
            h.model.request.clone(),
            h.model.preferences.recorder_sections.clone(),
        )
    });
    let point = cx.debug_bounds("audio-mode-advanced").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("audio-matrix-main-mic").is_some());
    cx.update(|_, cx| {
        let h = view.read(cx);
        assert!(h.audio_preferences().advanced);
        assert_eq!(h.model.request, before.0);
        assert_eq!(h.model.preferences.recorder_sections, before.1);
        let saved = serde_json::to_value(&h.model.preferences).unwrap();
        assert!(saved["audio_settings"].get("advanced").is_none());
        assert_eq!(
            h.settings_document.as_ref().unwrap().modes.audio,
            frame_settings::model::SettingsMode::Advanced
        );
        assert_eq!(saved["audio_settings"]["future_audio"], 42);
    });
    let point = cx.debug_bounds("audio-mode-easy").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("audio-easy-microphone").is_some());
    cx.update(|_, cx| assert_eq!(view.read(cx).model.request, before.0));
}

#[gpui::test]
fn matrix_click_changes_the_real_runtime_route_and_settles_request(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    view.update(cx, |home, cx| {
        setup(home, vec![source("mic", AudioRoute::MainMix)]);
        home.model.preferences.extra_preferences.insert(
            "audio_settings".into(),
            serde_json::json!({"advanced":true}),
        );
        cx.notify();
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("audio-matrix-own-mic").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    view.update(cx, |home, cx| {
        settle(home, cx);
        assert_eq!(
            home.services.control.published().audio_sources[0].route,
            AudioRoute::MainMixAndSeparate
        );
        assert_eq!(
            home.model.request.as_ref().unwrap().audio_routes[0].route,
            AudioRoute::MainMixAndSeparate
        );
        assert!(home.model.error.is_none());
    });
}

#[gpui::test]
fn easy_device_replacement_and_separate_option_apply_all_acknowledged_routes(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, cx| {
        setup(
            home,
            vec![
                source("mic-a", AudioRoute::MainMixAndSeparate),
                source("mic-b", AudioRoute::Ignore),
            ],
        );
        home.queue_audio_routes(
            vec![
                (AudioSourceId("mic-a".into()), AudioRoute::Ignore),
                (
                    AudioSourceId("mic-b".into()),
                    AudioRoute::MainMixAndSeparate,
                ),
            ],
            cx,
        );
        assert!(home.model.command_pending);
        assert_eq!(home.audio_ui.routes.len(), 1);
        settle(home, cx);
        let sources = home.services.control.published().audio_sources;
        assert_eq!(sources[0].route, AudioRoute::Ignore);
        assert_eq!(sources[1].route, AudioRoute::MainMixAndSeparate);
        home.queue_audio_routes(routing::separate_edits(&sources, false), cx);
        settle(home, cx);
        assert_eq!(
            home.model.request.as_ref().unwrap().audio_routes[1].route,
            AudioRoute::MainMix
        );
    });
}

#[gpui::test]
fn unavailable_sources_are_retained_and_cannot_be_replaced_by_a_default(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, _| {
        setup(home, vec![source("wasapi-mic:chosen", AudioRoute::MainMix)]);
        home.audio_ui.remember(&home.model.snapshot.audio_sources);
        home.model.snapshot.audio_sources = vec![source("wasapi-mic:other", AudioRoute::Ignore)];
        let sources = home.audio_sources();
        assert_eq!(sources.len(), 2);
        assert!(!sources[1].is_active);
        assert_eq!(sources[1].id.0, "wasapi-mic:chosen");
        assert_eq!(
            home.selected_audio_device(AudioSourceKind::Microphone, None)
                .unwrap()
                .0,
            "wasapi-mic:chosen"
        );
        assert!(home.validate_audio_recording().is_err());
        let routes = home.model.request.as_ref().unwrap().audio_routes.clone();
        let mut discovery = vec![source("wasapi-mic:other", AudioRoute::Ignore)];
        retain_saved_sources(&mut discovery, &routes);
        assert_eq!(discovery.len(), 2);
        assert!(!discovery[1].is_active);
    });
}

#[gpui::test]
fn routes_are_disabled_during_capture_but_audio_view_can_switch(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    view.update(cx, |home, cx| {
        setup(home, vec![source("mic", AudioRoute::MainMix)]);
        home.model.snapshot.state = RecorderState::Recording;
        cx.notify();
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("audio-mode-advanced").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    let point = cx.debug_bounds("audio-matrix-own-mic").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.update(|_, cx| {
        let h = view.read(cx);
        assert!(h.audio_preferences().advanced);
        assert_eq!(h.model.snapshot.audio_sources[0].route, AudioRoute::MainMix);
        assert!(!h.model.command_pending);
    });
}

#[gpui::test]
fn native_gain_click_settles_real_controls_and_keeps_recording_defaults(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    view.update(cx, |home, cx| {
        setup(home, vec![source("mic", AudioRoute::MainMixAndSeparate)]);
        home.model.request.as_mut().unwrap().optimization = Some(RecordingOptimization {
            encoder_overrides: VideoEncoderOverrides {
                rate_control: Some(VideoRateControl::Cqp),
                qp: Some(21),
                preset: Some(VideoEncoderPreset::P2),
                tuning: Some(VideoEncoderTuning::UltraLowLatency),
                gop_frames: Some(60),
                ..Default::default()
            },
            ..Default::default()
        });
        home.model.preferences.extra_preferences.insert(
            "audio_settings".into(),
            serde_json::json!({"advanced":true}),
        );
        cx.notify();
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("audio-gain-down-mic").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    view.update(cx, |home, cx| {
        assert!(home.model.command_pending);
        settle(home, cx);
        let saved = &home.model.request.as_ref().unwrap().audio_routes[0];
        assert_eq!(saved.controls.gain_millidb, -1000);
        assert_eq!(saved.controls.balance, 0);
        assert_eq!(
            saved.controls.reconnect,
            AudioReconnectPolicy::ProcessDefault
        );
        assert_eq!(saved.route, AudioRoute::MainMixAndSeparate);
        let video = home
            .model
            .request
            .as_ref()
            .unwrap()
            .optimization
            .as_ref()
            .unwrap();
        assert_eq!(
            video.encoder_overrides.rate_control,
            Some(VideoRateControl::Cqp)
        );
        assert_eq!(video.encoder_overrides.qp, Some(21));
        assert_eq!(video.encoder_overrides.preset, Some(VideoEncoderPreset::P2));
        assert_eq!(
            video.encoder_overrides.tuning,
            Some(VideoEncoderTuning::UltraLowLatency)
        );
        assert_eq!(video.encoder_overrides.gop_frames, Some(60));
        assert_eq!(
            home.services.control.published().audio_sources[0].controls,
            saved.controls
        );
        assert!(!home.model.command_pending);
    });
}
