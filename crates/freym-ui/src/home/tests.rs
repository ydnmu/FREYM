use super::*;
use gpui::{Modifiers, TestAppContext, size};

#[test]
fn ui_lab_defaults_migrate_only_appearance_and_keep_saved_recording_values() {
    let current = Preferences::default();
    assert_eq!(current.theme, crate::ui::theme::ThemeId::AmoledBlack);
    assert!(!current.artwork);
    assert!(current.filled_buttons);
    let mut old: Preferences = serde_json::from_str(
        r#"{"theme":"classic_light","artwork":true,"countdown":5,"future_setting":{"value":7}}"#,
    )
    .unwrap();
    assert!(old.migrate_appearance());
    assert_eq!(old.theme, crate::ui::theme::ThemeId::AmoledBlack);
    assert!(!old.artwork);
    assert_eq!(old.countdown, 5);
    assert_eq!(old.extra_preferences["future_setting"]["value"], 7);
    old.theme = crate::ui::theme::ThemeId::ClassicLight;
    assert!(!old.migrate_appearance());
    assert_eq!(old.theme, crate::ui::theme::ThemeId::ClassicLight);
}

#[test]
fn update_preference_migrates_persists_and_preserves_newer_preferences_on_rollback() {
    let mut prefs: Preferences =
        serde_json::from_str(r#"{"artwork":false,"futurePreference":{"enabled":true}}"#).unwrap();
    assert!(prefs.automatic_updates);
    assert!(!prefs.artwork);
    prefs.automatic_updates = false;
    let saved = serde_json::to_value(&prefs).unwrap();
    assert_eq!(saved["automatic_updates"], false);
    assert_eq!(saved["futurePreference"]["enabled"], true);
    let restored: Preferences = serde_json::from_value(saved).unwrap();
    assert!(!restored.automatic_updates);
}

pub(super) fn home(window: &mut Window, cx: &mut Context<Home>) -> Home {
    crate::ui::theme::ThemeId::ClassicLight.activate();
    crate::ui::theme::set_low_cost_effects(false);
    // Real production view; discovery, IO and hardware registration are not
    // bootstrapped. No mock recorder implementation replaces the engine.
    let (services, _) = Services::new();
    let focus = cx.focus_handle();
    window.focus(&focus, cx);
    let mut model = AppModel {
        connecting: false,
        ..Default::default()
    };
    model.preferences.artwork = false;
    cx.set_reduce_motion(true);
    Home {
        video: Default::default(),
        #[cfg(feature = "ui-preview")]
        ui_preview: false,
        model,
        services,
        focus,
        mic: cx.new(|_| AudioMeter::default()),
        system: cx.new(|_| AudioMeter::default()),
        meter_task: None,
        // Keep preference writes queued; headless UI fixtures never touch user settings.
        save_running: true,
        save_latest: None,
        settings_document: None,
        settings_committed: None,
        settings_category: 0,
        render_count: 0,
        hotkeys: None,
        shortcut_edit: None,
        number_editor: None,
        #[cfg(target_os = "windows")]
        preview: None,
        preview_enabled: false,
        studios: Default::default(),
        studio_prompt_pending: false,
        owner_window: window.window_handle(),
        start_intent: None,
        compact: None,
        compact_position: None,
        pending_mute: None,
        audio_ui: Default::default(),
        updates: None,
        update_snapshot: Default::default(),
        update_running: false,
        update_monitor: None,
        preferences_loaded: true,
    }
}

#[gpui::test]
fn settings_categories_fit_and_select_in_small_native_windows(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(home);
    for (width, height) in [(800., 640.), (1024., 768.), (1440., 900.)] {
        cx.simulate_resize(size(px(width), px(height)));
        view.update(cx, |home, cx| {
            home.model.sheet = Some(Sheet::Settings);
            home.settings_category = 0;
            cx.notify();
        });
        cx.run_until_parked();
        for (i, id) in [
            "settings-category-0",
            "settings-category-1",
            "settings-category-2",
            "settings-category-3",
            "settings-category-4",
            "settings-category-5",
            "settings-category-6",
            "settings-category-7",
        ]
        .into_iter()
        .enumerate()
        {
            let bounds = cx.debug_bounds(id).unwrap();
            assert!(
                bounds.left() >= px(0.)
                    && bounds.right() <= px(width)
                    && bounds.top() >= px(0.)
                    && bounds.bottom() <= px(height),
                "{bounds:?}"
            );
            cx.simulate_click(bounds.center(), Modifiers::default());
            cx.run_until_parked();
            assert_eq!(cx.update(|_, cx| view.read(cx).settings_category), i);
            assert_eq!(
                cx.update(|_, cx| view.read(cx).services.control.published().state),
                RecorderState::Idle
            );
        }
        cx.simulate_keystrokes("tab shift-tab escape");
        cx.run_until_parked();
        assert!(cx.update(|_, cx| view.read(cx).model.sheet.is_none()));
    }
}
#[gpui::test]
fn keyboard_settings_and_escape_do_not_start_or_mutate_a_recording(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-,");
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.sheet),
        Some(Sheet::Settings)
    );
    cx.simulate_keystrokes("escape ctrl-shift-r");
    cx.update(|_, cx| {
        let home = view.read(cx);
        assert_eq!(home.model.sheet, None);
        assert_eq!(home.services.control.published().state, RecorderState::Idle);
        assert!(!home.model.command_pending);
    });
}
#[gpui::test]
fn source_selection_and_disabled_record_use_native_hit_testing(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(home);
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    let source_card = cx
        .debug_bounds("source-settings")
        .expect("source settings bounds")
        .center();
    cx.simulate_click(source_card, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.update(|_, cx| view.read(cx).model.preferences.recorder_sections.video));
    let source_picker = cx
        .debug_bounds("inline-source-picker")
        .expect("inline source picker")
        .center();
    cx.simulate_click(source_picker, Modifiers::default());
    cx.run_until_parked();
    let source = cx
        .debug_bounds("Source(Window)")
        .expect("window selector bounds")
        .center();
    cx.simulate_click(source, Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let home = view.read(cx);
        assert_eq!(home.model.mode, SourceMode::Window);
        assert_eq!(home.model.sheet, Some(Sheet::Sources));
    });
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.sheet = None;
            cx.notify();
        })
    });
    cx.run_until_parked();
    let record = cx
        .debug_bounds("record-button")
        .expect("record bounds")
        .center();
    cx.simulate_click(record, Modifiers::default());
    cx.run_until_parked();
    assert!(!cx.update(|_, cx| view.read(cx).model.command_pending));
}
#[gpui::test]
fn settled_idle_home_has_no_permanent_animation_task(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let card = cx.debug_bounds("source-settings").unwrap().center();
    cx.simulate_mouse_move(card, None, Modifiers::default());
    cx.run_until_parked();
    let before = cx.update(|_, cx| view.read(cx).render_count);
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).render_count), before);
    cx.update(|_, cx| {
        assert!(view.read(cx).meter_task.is_none());
    });
}

#[gpui::test]
fn controls_remain_visible_across_supported_sizes_and_session_states(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(home);
    for (width, height) in [(800., 640.), (1280., 850.), (1920., 1080.)] {
        cx.simulate_resize(size(px(width), px(height)));
        for state in [
            RecorderState::Idle,
            RecorderState::Preparing,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Finalizing,
        ] {
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.model.preferences.last_recording = Some("recording.mkv".into());
                    home.model.preferences.last_replay = Some("replay.mkv".into());
                    home.model.snapshot.replay_active =
                        matches!(state, RecorderState::Recording | RecorderState::Paused);
                    home.model.snapshot.state = state;
                    home.model.error = Some("A long recoverable error message. ".repeat(80));
                    home.model.notice = Some("A warning message. ".repeat(20));
                    cx.notify();
                })
            });
            cx.run_until_parked();
            for id in [
                "record-button",
                "source-settings",
                "camera-settings",
                "quality-settings",
                "more-options",
                "Output",
                "home-settings",
                "capture-stage",
                "audio-panel",
                "StudioRecording",
                "StudioReplay",
            ] {
                let bounds = cx
                    .debug_bounds(id)
                    .unwrap_or_else(|| panic!("missing {id}"));
                assert!(
                    bounds.left() >= px(0.) && bounds.right() <= px(width),
                    "{id} horizontal bounds {bounds:?}"
                );
                assert!(
                    bounds.top() >= px(0.) && bounds.bottom() <= px(height),
                    "{id} vertical bounds {bounds:?}"
                );
            }
        }
    }
}

#[gpui::test]
fn source_card_event_selects_target_and_preserves_required_quality(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(home);
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            // Queue persistence without writing the user's preferences in this test.
            home.save_running = true;
            let display = DisplayInfo {
                id: DisplayId("display-test".into()),
                name: "Display 1".into(),
                width: 2560,
                height: 1440,
                x: 0,
                y: 0,
                scale_factor: 1.,
                refresh_rate_hz: Some(165.),
                is_primary: true,
                hdr_available: false,
                required_recording_quality: Some(Quality::High),
            };
            let mut snapshot = home.model.snapshot.clone();
            snapshot.displays = vec![display];
            home.model
                .initialize(snapshot, Default::default(), None, "recordings".into());
            home.model.sheet = Some(Sheet::Sources);
            cx.notify();
        })
    });
    cx.run_until_parked();
    let point = cx
        .debug_bounds("display-display-test")
        .expect("source card bounds")
        .center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let home = view.read(cx);
        let request = home.model.request.as_ref().unwrap();
        assert_eq!(
            request.target,
            CaptureTarget::Display(DisplayId("display-test".into()))
        );
        assert_eq!(request.quality, Quality::High);
        assert_eq!(home.model.sheet, None);
        assert!(home.save_latest.is_some());
        assert_eq!(home.services.control.published().state, RecorderState::Idle);
    });
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.request.as_mut().unwrap().fps_mode = FpsMode::Fixed(60.);
            cx.notify();
        })
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("quality-settings").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.update(|_, cx| view.read(cx).model.preferences.recorder_sections.quality));
    cx.update(|w, cx| view.update(cx, |home, cx| home.dispatch(Action::QualityOptions, w, cx)));
    cx.run_until_parked();
    let frame_rate_row = cx.debug_bounds("video-fps").unwrap();
    assert!(
        frame_rate_row.size.height <= px(crate::ui::theme::spacing::Layout::CONTROL_HEIGHT),
        "Frame rate must remain one horizontal setting row: {frame_rate_row:?}"
    );
    let before = cx.update(|_, cx| view.read(cx).model.request.clone());
    let point = cx.debug_bounds("video-mode-Advanced").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        before
    );
}

#[gpui::test]
fn nested_number_editor_preserves_input_focus_and_restores_settings_scope(cx: &mut TestAppContext) {
    use gpui::{Focusable, InputEvent};
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(home);
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            home.model.request = Some(RecordingRequest {
                target: CaptureTarget::Display(DisplayId("test".into())),
                fps_mode: FpsMode::AutoMatchSource,
                resolution: Resolution::Native,
                quality: Quality::Standard,
                encoder: EncoderPreference::Auto,
                optimization: None,
                audio_routes: vec![],
                camera: None,
                show_cursor: true,
                output: OutputSettings {
                    directory: "recordings".into(),
                    file_stem: "test".into(),
                },
            });
            home.model.sheet = Some(Sheet::Settings);
            home.edit_number(NumberPurpose::Fps, window, cx);
            cx.notify();
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let home = view.read(cx);
        let field = &home.number_editor.as_ref().unwrap().fields[0].1;
        assert!(field.focus_handle(cx).is_focused(window));
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.sheet),
        Some(Sheet::Settings)
    );
    // The restored settings scope starts at its close control, rather than
    // allowing Enter to activate a background recorder control.
    cx.simulate_keystrokes("enter");
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("enter").unwrap(),
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).model.sheet), None);
}
