use super::*;
use gpui::{Modifiers, TestAppContext, size};

#[gpui::test]
fn recorder_remove_disabled_audio_row_preserves_routes_and_can_be_added_again(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, cx| {
        let id = AudioSourceId("wasapi-mic:chosen".into());
        setup(home, vec![source(&id.0, AudioRoute::Ignore)]);
        home.model.preferences.extra_preferences.insert("audio_settings".into(),
            serde_json::json!({"microphone": id, "routes": {"wasapi-mic:chosen": "separateOnly"}, "future_audio": 42}));
        let before = (home.model.request.clone(), home.services.control.published().audio_sources,
            home.model.snapshot.audio_sources.clone());
        assert!(home.can_remove_recorder_audio_source(&id));
        home.remove_recorder_audio_source(id.clone(), cx);
        let saved = serde_json::to_value(&home.model.preferences).unwrap();
        home.model.preferences = serde_json::from_value(saved).unwrap();
        assert!(home.recorder_audio_sources().is_empty(), "removal survives preference reload");
        assert_eq!(home.additional_audio_sources()[0].id, id);
        assert_eq!(home.model.request, before.0);
        assert_eq!(home.services.control.published().audio_sources, before.1);
        assert_eq!(home.model.snapshot.audio_sources, before.2);
        assert!(home.model.pending_route.is_none() && home.audio_ui.routes.is_empty());
        assert_eq!(home.audio_preferences().extra["future_audio"], 42);
        home.add_recorder_audio_source(id.clone(), cx);
        assert_eq!(home.recorder_audio_sources()[0].id, id);
        assert_eq!(home.enabled_audio_route(&id), AudioRoute::SeparateOnly);
        home.model.snapshot.audio_sources[0].route = AudioRoute::MainMix;
        let before_prefs = serde_json::to_value(&home.model.preferences).unwrap();
        home.remove_recorder_audio_source(id.clone(), cx);
        assert_eq!(serde_json::to_value(&home.model.preferences).unwrap(), before_prefs);
        home.model.snapshot.audio_sources[0].route = AudioRoute::Ignore;
        home.model.snapshot.state = RecorderState::Recording;
        home.remove_recorder_audio_source(id.clone(), cx);
        assert_eq!(serde_json::to_value(&home.model.preferences).unwrap(), before_prefs);
        assert!(home.recorder_audio_sources().iter().any(|s| s.id == id));
    });
}
fn expand_processing(cx: &mut gpui::VisualTestContext) {
    assert!(cx.debug_bounds("audio-source-processing-toggle").is_none());
    assert!(cx.debug_bounds("audio-source-route").is_some());
}

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

#[gpui::test]
fn recorder_summary_preserves_selected_devices_and_every_enabled_route(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, cx| {
        let mut elgato = source("wasapi-mic:elgato", AudioRoute::SeparateOnly);
        elgato.display_name = "Elgato Wave Link".into();
        elgato.muted = true;
        elgato.controls.gain_millidb = -6000;
        setup(
            home,
            vec![
                source("wasapi-mic:extra", AudioRoute::Ignore),
                source("wasapi-mic:chosen", AudioRoute::Ignore),
                elgato,
                AudioSource {
                    kind: AudioSourceKind::System,
                    ..source("wasapi-loopback:chosen", AudioRoute::MainMix)
                },
                AudioSource {
                    kind: AudioSourceKind::System,
                    ..source("wasapi-loopback:extra", AudioRoute::Ignore)
                },
                AudioSource {
                    kind: AudioSourceKind::Application,
                    ..source("app:enabled", AudioRoute::MainMixAndSeparate)
                },
                AudioSource {
                    kind: AudioSourceKind::Application,
                    ..source("app:browser", AudioRoute::Ignore)
                },
            ],
        );
        home.model.preferences.extra_preferences.insert("audio_settings".into(), serde_json::json!({
            "microphone":"wasapi-mic:chosen", "system":"wasapi-loopback:chosen", "future_audio":42
        }));
        let before = (
            home.model.request.clone(),
            home.model.snapshot.audio_sources.clone(),
            home.model.preferences.clone(),
        );
        let visible = home.recorder_audio_sources();
        assert_eq!(
            visible.iter().map(|s| s.id.0.as_str()).collect::<Vec<_>>(),
            vec![
                "wasapi-mic:chosen",
                "wasapi-mic:elgato",
                "wasapi-loopback:chosen",
                "app:enabled"
            ]
        );
        assert!(visible[1].muted);
        assert_eq!(visible[1].controls.gain_millidb, -6000);
        let additional = home.additional_audio_sources();
        assert_eq!(additional.len(), 3);
        // Per-application capability is not a browser-tab capture contract.
        assert!(!additional.iter().any(|s| s.display_name == "Tab Audio"));
        home.model.snapshot.capabilities.per_app_audio = false;
        assert_eq!(home.additional_audio_sources().len(), 2);
        assert!(
            home.recorder_audio_sources()
                .iter()
                .any(|s| s.id.0 == "app:enabled")
        );
        let _ = home.home_audio_panel(cx);
        assert_eq!(home.model.request, before.0);
        assert_eq!(home.model.snapshot.audio_sources, before.1);
        assert_eq!(
            serde_json::to_value(&home.model.preferences).unwrap(),
            serde_json::to_value(&before.2).unwrap()
        );
        assert!(home.save_latest.is_none());
        assert!(home.audio_ui.routes.is_empty());
        assert!(home.model.pending_route.is_none());
    });
}

#[gpui::test]
fn recorder_add_source_stages_only_visibility_and_survives_disconnect_and_reload(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, cx| {
        setup(
            home,
            vec![
                source("wasapi-mic:primary", AudioRoute::Ignore),
                source("wasapi-mic:elgato", AudioRoute::Ignore),
            ],
        );
        let before = (
            home.model.request.clone(),
            home.services.control.published().audio_sources,
        );
        let id = AudioSourceId("wasapi-mic:elgato".into());
        home.add_recorder_audio_source(id.clone(), cx);
        home.add_recorder_audio_source(id.clone(), cx);
        assert_eq!(home.audio_preferences().recorder_sources, vec![id.clone()]);
        assert!(home.save_latest.is_some());
        assert_eq!(home.model.request, before.0);
        assert_eq!(home.services.control.published().audio_sources, before.1);
        assert!(home.audio_ui.routes.is_empty());
        assert!(home.model.pending_route.is_none());
        let saved = serde_json::to_value(&home.model.preferences).unwrap();
        home.model.preferences = serde_json::from_value(saved).unwrap();
        home.model.snapshot.audio_sources.retain(|s| s.id != id);
        home.audio_ui = Default::default();
        let visible = home.recorder_audio_sources();
        let missing = visible.iter().find(|s| s.id == id).unwrap();
        assert!(!missing.is_active);
        assert_eq!(missing.route, AudioRoute::Ignore);
        home.model
            .snapshot
            .audio_sources
            .push(source(&id.0, AudioRoute::Ignore));
        assert!(
            home.recorder_audio_sources()
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .is_active
        );
        home.model.snapshot.state = RecorderState::Recording;
        let prefs = serde_json::to_value(home.audio_preferences()).unwrap();
        home.add_recorder_audio_source(AudioSourceId("unknown".into()), cx);
        assert_eq!(
            serde_json::to_value(home.audio_preferences()).unwrap(),
            prefs
        );
    });
}

#[gpui::test]
fn recorder_disabled_selection_never_falls_back_when_its_device_is_missing(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, _| {
        setup(home, vec![source("wasapi-mic:other", AudioRoute::Ignore)]);
        home.model.preferences.extra_preferences.insert(
            "audio_settings".into(),
            serde_json::json!({
                "microphone":"wasapi-mic:missing", "system":"wasapi-loopback:missing",
                "routes":{"wasapi-mic:elgato":"separateOnly"}
            }),
        );
        let visible = home.recorder_audio_sources();
        assert_eq!(visible.len(), 3);
        assert!(
            visible
                .iter()
                .all(|s| !s.is_active && s.route == AudioRoute::Ignore)
        );
        assert_eq!(home.additional_audio_sources()[0].id.0, "wasapi-mic:other");
        assert!(
            home.model
                .request
                .as_ref()
                .unwrap()
                .audio_routes
                .iter()
                .all(|r| r.route == AudioRoute::Ignore)
        );
    });
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
        audio_encoding: Default::default(),
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
    // Production snapshot delivery also notifies the view after settlement.
    cx.notify();
}

#[gpui::test]
fn audio_enable_restores_separate_route_without_changing_mute_or_processing(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(super::super::tests::home);
    view.update(cx, |home, cx| {
        let mut mic = source("mic", AudioRoute::SeparateOnly);
        mic.muted = true;
        mic.controls.gain_millidb = -6000;
        let id = mic.id.clone();
        setup(home, vec![mic]);
        home.route(id.clone(), AudioRoute::Ignore, cx);
        settle(home, cx);
        assert_eq!(
            home.model.snapshot.audio_sources[0].route,
            AudioRoute::Ignore
        );
        let prefs = serde_json::to_value(home.audio_preferences()).unwrap();
        let restored: AudioPreferences = serde_json::from_value(prefs).unwrap();
        assert_eq!(restored.routes[&id], AudioRoute::SeparateOnly);
        let next = home.enabled_audio_route(&id);
        home.route(id, next, cx);
        settle(home, cx);
        let mic = &home.model.snapshot.audio_sources[0];
        assert_eq!(mic.route, AudioRoute::SeparateOnly);
        assert!(mic.muted);
        assert_eq!(mic.controls.gain_millidb, -6000);
        assert!(home.validate_audio_recording().is_ok());
    });
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
    expand_processing(cx);
    assert!(cx.debug_bounds("audio-source-route").is_some());
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
    expand_processing(cx);
    let point = cx.debug_bounds("audio-source-route").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    if let Some(option) = cx.debug_bounds("audio-source-route-option-3") {
        cx.simulate_click(option.center(), Modifiers::default());
    }
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
    expand_processing(cx);
    let point = cx.debug_bounds("audio-source-route").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    if let Some(option) = cx.debug_bounds("audio-source-route-option-3") {
        cx.simulate_click(option.center(), Modifiers::default());
    }
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
    expand_processing(cx);
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
