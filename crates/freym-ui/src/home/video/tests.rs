use super::super::*;
use crate::video::{VideoAction, VideoEdit, VideoMode, VideoSection, adapter::tests::fixture};
use gpui::{AppContext, Modifiers, TestAppContext, size};

fn fixture_home(window: &mut Window, cx: &mut Context<Home>) -> Home {
    let mut home = super::super::tests::home(window, cx);
    let (request, caps) = fixture();
    home.model.snapshot.displays = vec![DisplayInfo {
        id: DisplayId("test-display".into()),
        name: "Fixture display".into(),
        width: 1920,
        height: 1080,
        x: 0,
        y: 0,
        scale_factor: 1.,
        refresh_rate_hz: Some(60.),
        is_primary: true,
        hdr_available: false,
        required_recording_quality: None,
    }];
    // Discovery stays off; this headless test supplies explicit fixture capabilities.
    home.model.snapshot.capabilities.fixed_fps = true;
    home.model.request = Some(request);
    home.model.sheet = Some(Sheet::Settings);
    home.settings_category = 1;
    home.video.capabilities = Some(caps);
    home
}

#[test]
fn automatic_video_captions_use_only_a_valid_qualified_plan() {
    use super::super::video_screen::{automatic_codec, automatic_encoder, automatic_quality};
    let (request, caps) = fixture();
    let before = serde_json::to_value(&request).unwrap();
    let plan = caps.plan(&request).unwrap();
    assert_eq!(automatic_codec(Some(&plan)), "Automatic (H.264)");
    assert_eq!(
        automatic_encoder(Some(&caps), Some(&plan)),
        "Automatic (NVENC fixture)"
    );
    assert_eq!(
        automatic_quality(Some(&plan)),
        format!("Automatic ({:?})", plan.quality)
    );
    assert_eq!(automatic_codec(None), "Automatic (pending)");
    assert_eq!(automatic_encoder(None, Some(&plan)), "Automatic (pending)");
    assert_eq!(automatic_quality(None), "Automatic (pending)");
    let mut unknown = plan.clone();
    unknown.encoder.id = "unqualified".into();
    assert_eq!(
        automatic_encoder(Some(&caps), Some(&unknown)),
        "Automatic (pending)"
    );
    let mut invalid = request.clone();
    invalid.fps_mode = FpsMode::Fixed(f64::NAN);
    assert!(caps.plan(&invalid).is_err());
    assert_eq!(
        automatic_codec(caps.plan(&invalid).as_ref().ok()),
        "Automatic (pending)"
    );
    assert_eq!(serde_json::to_value(&request).unwrap(), before);
}

#[gpui::test]
fn paired_capture_controls_and_dropdowns_follow_their_trigger_width(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(fixture_home);
    view.update(cx, |home, cx| {
        home.model.sheet = Some(Sheet::Settings);
        home.settings_category = 1;
        cx.notify();
    });
    let request = view.read_with(cx, |home, _| {
        serde_json::to_value(&home.model.request).unwrap()
    });
    for (width, height) in [(800., 640.), (1280., 850.), (1700., 1000.)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        let res = cx.debug_bounds("video-resolution").unwrap();
        let fps = cx.debug_bounds("video-fps").unwrap();
        let pair = cx.debug_bounds("video-capture-controls").unwrap();
        assert!((res.center().y - fps.center().y).abs() <= px(1.));
        assert!(res.right() < fps.left());
        assert!((fps.left() - res.right() - px(8.)).abs() < px(1.));
        assert!(
            res.size.width >= px(104.) && res.size.height <= px(41.),
            "resolution must fit its full value: {res:?}"
        );
        assert!(res.left() >= pair.left() && fps.right() <= pair.right());
        for (id, menu_id) in [
            ("video-resolution", "video-resolution-menu"),
            ("video-fps", "video-fps-menu"),
            ("video-codec", "video-codec-menu"),
        ] {
            // These rows fit without scrolling after the combined Capture row.
            let trigger = cx.debug_bounds(id).unwrap();
            cx.simulate_click(trigger.center(), Modifiers::default());
            cx.run_until_parked();
            let menu = cx.debug_bounds(menu_id).unwrap();
            assert!(
                (menu.size.width - trigger.size.width).abs() <= px(2.),
                "{id} menu={menu:?}, trigger={trigger:?}"
            );
            assert!((menu.center().x - trigger.center().x).abs() <= px(1.));
            assert!(menu.bottom() <= px(height) - px(7.) && menu.left() >= px(7.));
            let below = px(height) - trigger.bottom();
            if below >= px(96.) {
                assert!(
                    menu.top() >= trigger.bottom(),
                    "menu should open below {id}"
                );
            }
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(cx.debug_bounds(menu_id).is_none());
        }
    }
    assert_eq!(
        view.read_with(cx, |home, _| serde_json::to_value(&home.model.request)
            .unwrap()),
        request
    );
}

#[gpui::test]
fn native_rate_menu_applies_independent_cbr_and_queues_shared_save(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.video.mode = VideoMode::Advanced;
            home.video.expanded[VideoSection::Rate.index()] = true;
            home.model.request.as_mut().unwrap().audio_routes = vec![AudioRouteSelection {
                source_id: AudioSourceId("wasapi-mic:exact".into()),
                route: AudioRoute::MainMixAndSeparate,
                muted: true,
                controls: AudioSourceControls {
                    gain_millidb: -4500,
                    balance: 35,
                    reconnect: AudioReconnectPolicy::ExactDevice,
                },
            }];
            let caps = home.video.capabilities.as_mut().unwrap();
            // Explicit headless fixture evidence; actual hardware evidence is separate.
            caps.encoders[0].native_options.push(VideoEncoderOverrides {
                rate_control: Some(VideoRateControl::Cbr),
                ..Default::default()
            });
            cx.notify();
        })
    });
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let viewport = cx.debug_bounds("settings-page-body").unwrap();
    let target = cx.debug_bounds("video-rate").unwrap();
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: viewport.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(
            px(0.),
            viewport.center().y - target.center().y,
        )),
        modifiers: Default::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
    let trigger = cx.debug_bounds("video-rate").unwrap().center();
    let scope = cx.update(|_, cx| {
        let home = view.read(cx);
        (
            home.model.editable(),
            home.model.settings_blocked,
            home.video_context()
                .as_ref()
                .is_some_and(|c| home.video.capabilities_for(c).is_some()),
        )
    });
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    let cbr = cx
        .debug_bounds("video-rate-option-1")
        .unwrap_or_else(|| {
            panic!(
                "rate menu absent; trigger={trigger:?}, scope={scope:?}; menu={:?}",
                cx.debug_bounds("video-rate-menu")
            )
        })
        .center();
    cx.simulate_click(cbr, Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let home = view.read(cx);
        let request = home.model.request.as_ref().unwrap();
        assert_eq!(request.quality, Quality::High);
        assert_eq!(request.audio_routes[0].source_id.0, "wasapi-mic:exact");
        assert_eq!(
            request.audio_routes[0].route,
            AudioRoute::MainMixAndSeparate
        );
        assert!(request.audio_routes[0].muted);
        assert_eq!(request.audio_routes[0].controls.gain_millidb, -4500);
        assert_eq!(request.audio_routes[0].controls.balance, 35);
        assert_eq!(
            request.audio_routes[0].controls.reconnect,
            AudioReconnectPolicy::ExactDevice
        );
        assert_eq!(
            request
                .optimization
                .as_ref()
                .unwrap()
                .encoder_overrides
                .rate_control,
            Some(VideoRateControl::Cbr)
        );
        assert_eq!(
            request.optimization.as_ref().unwrap().bitrate_bps,
            Some(30_000_000)
        );
        assert!(home.save_latest.is_some());
        assert_eq!(home.services.control.published().state, RecorderState::Idle);
    });
}

#[gpui::test]
fn custom_qp_editor_rejects_out_of_range_and_preserves_unrelated_values(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            let options = home
                .model
                .request
                .as_mut()
                .unwrap()
                .optimization
                .as_mut()
                .unwrap();
            options.bitrate_bps = None;
            options.encoder_overrides = VideoEncoderOverrides {
                rate_control: Some(VideoRateControl::Cqp),
                qp: Some(23),
                ..Default::default()
            };
            let context = home.video_context().unwrap();
            home.video.capabilities.as_mut().unwrap().context = context;
            let before = home.model.request.clone();
            home.edit_number(NumberPurpose::VideoQp, window, cx);
            home.number_editor.as_mut().unwrap().fields[0].1 =
                cx.new(|cx| crate::input::TextInput::new("52", "", cx));
            home.apply_video_number(window, cx);
            assert_eq!(home.model.request, before);
            assert!(home.model.error.is_some());
            assert!(home.save_latest.is_none());
            home.number_editor.as_mut().unwrap().fields[0].1 =
                cx.new(|cx| crate::input::TextInput::new("17", "", cx));
            home.apply_video_number(window, cx);
            let request = home.model.request.as_ref().unwrap();
            assert_eq!(
                request.optimization.as_ref().unwrap().encoder_overrides.qp,
                Some(17)
            );
            assert_eq!(request.output, before.unwrap().output);
            assert!(home.number_editor.is_none());
            assert!(home.save_latest.is_some());
        })
    });
}

#[gpui::test]
fn native_mode_switch_and_open_groups_preserve_values_at_supported_sizes(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    let original = cx.update(|_, cx| view.read(cx).model.request.clone());
    for (width, height) in [(800., 640.), (1280., 850.), (1920., 1080.)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        for id in [
            "video-resolution",
            "video-fps",
            "video-quality",
            "video-codec",
            "video-encoder",
        ] {
            let bounds = cx.debug_bounds(id).expect("easy control bounds");
            assert!(
                bounds.origin.x >= px(0.) && bounds.right() <= px(width),
                "{id}: {bounds:?}"
            );
            assert!(
                bounds.size.width
                    >= px(match id {
                        "video-fps" => 104.,
                        "video-resolution" => 104.,
                        _ => 200.,
                    }),
                "{id} at {width}: {bounds:?}"
            );
        }
        let advanced = cx.debug_bounds("video-mode-Advanced").unwrap().center();
        cx.simulate_click(advanced, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| view.read(cx).video.mode),
            VideoMode::Advanced
        );
        assert!(cx.debug_bounds("video-section-Geometry").is_none());
        assert!(cx.debug_bounds("video-capture-controls").is_some());
        assert!(cx.debug_bounds("video-section-Capture").is_none());
        assert!(cx.debug_bounds("video-rate").is_some());
        assert!(cx.debug_bounds("video-fps").is_some());
        assert!(!cx.update(|_, cx| view.read(cx).video.expanded[VideoSection::Geometry.index()]));
        let easy = cx.debug_bounds("video-mode-Easy").unwrap().center();
        cx.simulate_click(easy, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| view.read(cx).model.request.clone()),
            original
        );
        // Rendering open groups never mutates the saved disclosure state.
        cx.update(|_, cx| {
            view.update(cx, |home, cx| {
                home.video.expanded = [false; 7];
                cx.notify();
            })
        });
    }
}

#[gpui::test]
fn full_page_settings_escape_closes_only_the_focused_video_menu(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.sheet = Some(Sheet::Settings);
            home.settings_category = 1;
            cx.notify();
        })
    });
    cx.run_until_parked();
    let before = cx.update(|_, cx| serde_json::to_value(&view.read(cx).model.request).unwrap());
    let trigger = cx
        .debug_bounds("video-fps")
        .expect("frame rate trigger")
        .center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-fps-menu").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-fps-menu").is_none());
    cx.update(|_, cx| {
        assert_eq!(view.read(cx).model.sheet, Some(Sheet::Settings));
        assert_eq!(
            serde_json::to_value(&view.read(cx).model.request).unwrap(),
            before
        );
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).model.sheet), None);
}

#[gpui::test]
fn native_dropdown_selects_real_encoder_and_escape_only_closes_menu(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let trigger = cx.debug_bounds("video-encoder").unwrap().center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    let menu = cx
        .debug_bounds("video-encoder-menu")
        .expect("native menu bounds");
    assert!(menu.origin.x >= px(0.) && menu.right() <= px(1280.));
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-encoder-menu").is_none());
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-encoder-menu").is_none());
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.sheet),
        Some(Sheet::Settings)
    );
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    let encoder = cx.debug_bounds("video-encoder-option-1").unwrap().center();
    let current_menu = cx.debug_bounds("video-encoder-menu").unwrap();
    assert!(
        current_menu.contains(&encoder),
        "menu={current_menu:?}; option={encoder:?}"
    );
    cx.simulate_click(encoder, Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let home = view.read(cx);
        assert_eq!(
            home.model.request.as_ref().unwrap().encoder,
            EncoderPreference::Explicit(
                home.video.capabilities.as_ref().unwrap().encoders[0]
                    .candidate
                    .id
                    .clone()
            ),
            "error={:?}; menu hit at {encoder:?}; queued save={}",
            home.model.error,
            home.save_latest.is_some()
        );
        assert!(home.save_latest.is_some());
        assert_eq!(home.services.control.published().state, RecorderState::Idle);
    });
}

#[gpui::test]
fn unavailable_codec_cannot_mutate_values_and_dropdown_dismisses_outside(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let original = cx.update(|_, cx| view.read(cx).model.request.clone());
    let trigger = cx.debug_bounds("video-codec").unwrap().center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    let unsupported = cx.debug_bounds("video-codec-option-3").unwrap().center();
    cx.simulate_click(unsupported, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        original
    );
    let outside = cx.debug_bounds("video-mode-Easy").unwrap().center();
    cx.simulate_click(outside, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-codec-menu").is_none());
    cx.update(|w, cx| {
        view.update(cx, |home, cx| {
            home.dispatch_video(
                VideoAction::Edit(VideoEdit::Codec(RecordingCodec::Av1)),
                w,
                cx,
            )
        })
    });
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        original
    );
    assert!(cx.update(|_, cx| view.read(cx).model.error.is_some()));
}

#[gpui::test]
fn disabled_video_controls_cannot_change_a_running_recording(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.snapshot.state = RecorderState::Recording;
            cx.notify();
        })
    });
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let original = cx.update(|_, cx| view.read(cx).model.request.clone());
    let trigger = cx.debug_bounds("video-quality").unwrap().center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("video-quality-menu").is_none());
    cx.update(|w, cx| {
        view.update(cx, |home, cx| {
            home.dispatch_video(
                VideoAction::Edit(VideoEdit::Quality(Some(Quality::Standard))),
                w,
                cx,
            )
        })
    });
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        original
    );
    assert!(cx.update(|_, cx| view.read(cx).save_latest.is_none()));
}

#[gpui::test]
fn keyboard_menu_navigation_applies_quality_and_leaves_no_idle_animation(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    let trigger = cx.debug_bounds("video-quality").unwrap().center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.as_ref().unwrap().quality),
        Quality::Maximum
    );
    assert!(cx.debug_bounds("video-quality-menu").is_none());
    let before = cx.update(|_, cx| view.read(cx).render_count);
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).render_count), before);
}

#[gpui::test]
fn invalid_manual_video_values_are_refused_before_ui_record_admission(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.update(|_, cx| {
        view.update(cx, |home, _| {
            assert!(home.video_start_error().is_none());
            home.model.request.as_mut().unwrap().encoder =
                EncoderPreference::Explicit("removed-encoder".into());
            assert!(home.video_start_error().is_some());
            home.video.capabilities = None;
            assert!(
                home.video_start_error()
                    .unwrap()
                    .contains("Check Video hardware")
            );
            assert_eq!(home.services.control.published().state, RecorderState::Idle);
        })
    });
}

#[gpui::test]
fn video_number_editors_reject_invalid_values_without_queuing_a_save(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    let original = cx.update(|_, cx| view.read(cx).model.request.clone());
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            home.edit_number(NumberPurpose::Bitrate, window, cx);
            let invalid = cx.new(|cx| crate::input::TextInput::new(u32::MAX.to_string(), "", cx));
            home.number_editor.as_mut().unwrap().fields[0].1 = invalid;
            home.apply_video_number(window, cx);
            assert!(home.number_editor.is_some());
            assert!(home.model.error.is_some());
            assert!(home.save_latest.is_none());
        })
    });
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        original
    );
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            home.edit_number(NumberPurpose::Fps, window, cx);
            let invalid = cx.new(|cx| crate::input::TextInput::new("NaN", "", cx));
            home.number_editor.as_mut().unwrap().fields[0].1 = invalid;
            home.apply_video_number(window, cx);
            assert!(home.number_editor.is_some());
            assert!(home.model.error.is_some());
            assert!(home.save_latest.is_none());
        })
    });
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.clone()),
        original
    );
}

#[gpui::test]
fn video_dropdowns_and_disclosures_fit_every_freym_theme(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(800.), px(640.)));
    for theme in crate::ui::theme::ThemeId::ALL {
        cx.update(|_, cx| {
            view.update(cx, |home, cx| {
                theme.activate();
                home.model.preferences.theme = theme;
                home.video.mode = VideoMode::Easy;
                cx.refresh_windows();
                cx.notify();
            })
        });
        cx.run_until_parked();
        let scroll = |cx: &mut gpui::VisualTestContext, lines| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::point(px(550.), px(400.)),
                delta: gpui::ScrollDelta::Lines(gpui::point(0., lines)),
                modifiers: Default::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
        };
        scroll(cx, 1000.);
        for _ in 0..12 {
            let encoder = cx.debug_bounds("video-encoder").unwrap();
            if encoder.top() >= px(100.) && encoder.bottom() <= px(580.) {
                break;
            }
            scroll(cx, -2.);
        }
        let encoder = cx
            .debug_bounds("video-encoder")
            .expect("encoder fits theme");
        cx.simulate_click(encoder.center(), Modifiers::default());
        cx.run_until_parked();
        let menu = cx.debug_bounds("video-encoder-menu").unwrap();
        assert!(
            menu.left() >= px(0.)
                && menu.right() <= px(800.)
                && menu.top() >= px(0.)
                && menu.bottom() <= px(640.),
            "{theme:?}: {menu:?}"
        );
        cx.simulate_click(encoder.center(), Modifiers::default());
        cx.run_until_parked();
        scroll(cx, 1000.);
        let advanced = cx.debug_bounds("video-mode-Advanced").unwrap().center();
        cx.simulate_click(advanced, Modifiers::default());
        cx.run_until_parked();
        let bounds = cx.debug_bounds("video-capture-controls").unwrap();
        assert!(
            bounds.left() >= px(0.) && bounds.right() <= px(800.),
            "{theme:?}: {bounds:?}"
        );
    }
}

#[gpui::test]
fn keyboard_end_scrolls_long_fps_menu_and_opens_the_custom_editor(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(fixture_home);
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    let trigger = cx.debug_bounds("video-fps").unwrap().center();
    cx.simulate_click(trigger, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    let menu = cx.debug_bounds("video-fps-menu").unwrap();
    let option = cx.debug_bounds("video-fps-option-7").unwrap();
    assert!(
        menu.contains(&option.center()),
        "menu={menu:?}; custom={option:?}"
    );
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|_, cx| view.read(cx).number_editor.is_some()));
    assert_eq!(
        cx.update(|_, cx| view.read(cx).model.request.as_ref().unwrap().fps_mode),
        FpsMode::Fixed(60.)
    );
}
