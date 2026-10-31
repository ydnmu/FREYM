//! Headless GPUI checks; no desktop automation, capture, or manual visual acceptance.
use super::*;
use crate::ui::theme::{ThemeId, colors::Color, motion};
use gpui::{TestAppContext, size};

struct ResetTheme;

#[gpui::test]
fn finishing_countdown_cycles_and_custom_edits_without_popup_or_reflow(cx: &mut TestAppContext) {
    use gpui::Modifiers;
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(super::tests::home);
    for (width, height) in [(800., 640.), (1280., 850.)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        let stable = [
            "record-primary-transport",
            "home-settings",
            "workspace-reset",
            "command-deck-output",
        ]
        .map(|id| cx.debug_bounds(id).unwrap());
        for seconds in [1, 3, 5, 10, 0] {
            let point = cx.debug_bounds("home-countdown").unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
            assert_eq!(
                view.read_with(cx, |h, _| h.model.preferences.countdown),
                seconds
            );
            assert!(cx.debug_bounds("countdown-menu").is_none());
            assert!(cx.debug_bounds("countdown-value").is_some());
            assert_eq!(
                [
                    "record-primary-transport",
                    "home-settings",
                    "workspace-reset",
                    "command-deck-output"
                ]
                .map(|id| cx.debug_bounds(id).unwrap()),
                stable
            );
        }
    }
    let point = cx.debug_bounds("home-countdown").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    let point = cx.debug_bounds("countdown-value").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |h, _| h.model.preferences.countdown),
        1,
        "number edits, never cycles"
    );
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("999");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.debug_bounds("countdown-invalid").is_some());
    assert_eq!(view.read_with(cx, |h, _| h.model.preferences.countdown), 1);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("17");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |h, _| h.model.preferences.countdown), 17);
    assert!(view.read_with(cx, |h, _| h.save_latest.is_some()));
    let point = cx.debug_bounds("countdown-value").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("22");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |h, _| h.model.preferences.countdown), 17);
    view.update(cx, |h, cx| {
        h.model.snapshot.state = RecorderState::Recording;
        cx.notify();
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("home-countdown").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |h, _| h.model.preferences.countdown), 17);
    assert!(cx.debug_bounds("countdown-custom").is_none());
}

impl Drop for ResetTheme {
    fn drop(&mut self) {
        ThemeId::White.activate();
        crate::ui::theme::set_low_cost_effects(false);
        crate::ui::theme::SurfacePreference::Frosted.activate();
        crate::ui::theme::surface_treatment::set_frosted_transparency(20);
    }
}

#[gpui::test]
fn command_deck_directory_opens_separately_from_the_existing_folder_picker(
    cx: &mut TestAppContext,
) {
    use gpui::Modifiers;
    let (view, cx) = cx.add_window_view(super::tests::home);
    view.update(cx, |home, cx| {
        home.model.request = Some(RecordingRequest {
            target: CaptureTarget::Display(DisplayId("fixture".into())),
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
                directory: "C:/FREYM/build/deck-bottom-20261027".into(),
                file_stem: "fixture".into(),
            },
        });
        cx.notify();
    });
    cx.simulate_resize(size(px(1280.), px(850.)));
    cx.run_until_parked();
    let folder = cx.debug_bounds("open-output-directory").unwrap().center();
    cx.simulate_click(folder, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        super::shell::OPENED_OUTPUT
            .with(|path| path.borrow().clone())
            .as_deref(),
        Some("C:/FREYM/build/deck-bottom-20261027")
    );
    super::shell::OPENED_OUTPUT.with(|path| *path.borrow_mut() = None);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        super::shell::OPENED_OUTPUT.with(|path| path.borrow().is_some()),
        "directory link is keyboard accessible"
    );
    assert!(
        !cx.did_prompt_for_paths(),
        "opening a directory must not invoke its picker"
    );
    let chooser = cx.debug_bounds("Output").unwrap().center();
    cx.simulate_click(chooser, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories && !options.files && !options.multiple);
        Some(vec![
            "C:/FREYM/build/deck-bottom-20261027/selected-fixture".into(),
        ])
    });
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |home, _| home.output_directory().unwrap().to_owned()),
        "C:/FREYM/build/deck-bottom-20261027/selected-fixture"
    );
    assert!(
        view.read_with(cx, |home, _| home.save_latest.is_some()),
        "selection uses existing queued persistence"
    );
    view.update(cx, |home, cx| {
        home.model.snapshot.state = RecorderState::Recording;
        cx.notify();
    });
    cx.run_until_parked();
    let chooser = cx.debug_bounds("Output").unwrap().center();
    cx.simulate_click(chooser, Modifiers::default());
    cx.run_until_parked();
    assert!(
        !cx.did_prompt_for_paths(),
        "recording must gate directory changes"
    );
}

#[gpui::test]
fn settings_uses_the_same_single_layer_panel_compositor_as_recorder(cx: &mut TestAppContext) {
    use crate::ui::{
        components::PanelBackgrounds,
        theme::{SurfacePreference, surface_treatment},
    };
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    SurfacePreference::Frosted.activate();
    if !surface_treatment::is_frosted() {
        return;
    }
    cx.simulate_resize(size(px(1280.), px(850.)));
    for theme in ThemeId::ALL {
        theme.activate();
        for percent in [20, 50, 100] {
            surface_treatment::set_frosted_transparency(percent);
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.model.sheet = None;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            assert_eq!(PanelBackgrounds::painted_panels().len(), 4);
            let material = Color::DockSurface.get();
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.model.sheet = Some(Sheet::Settings);
                    cx.notify();
                })
            });
            cx.run_until_parked();
            assert_eq!(Color::DockSurface.get(), material);
            let panels = PanelBackgrounds::painted_panels();
            assert_eq!(
                panels.len(),
                2,
                "Settings panels must replace the shell beneath them"
            );
            for id in ["settings-sidebar", "settings-content-panel"] {
                let panel = cx.debug_bounds(id).unwrap();
                assert!(
                    panels
                        .iter()
                        .any(|(b, r)| (b.center().x - panel.center().x).abs() <= px(1.)
                            && (b.center().y - panel.center().y).abs() <= px(1.)
                            && *r == px(14.))
                );
            }
        }
    }
}

#[gpui::test]
fn command_deck_compact_controls_keep_recording_notifications_in_reserved_space(
    cx: &mut TestAppContext,
) {
    use gpui::Modifiers;
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.snapshot.last_artifact = None;
            home.model
                .interrupted
                .push("fixture-interrupted.mkv".into());
            cx.notify();
        })
    });
    for (width, height) in [(800., 640.), (1280., 850.), (1700., 1000.)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("dock-panel-controls").unwrap();
        let footprint = cx.debug_bounds("record-primary-transport").unwrap();
        assert!(footprint.size.width <= panel.size.width);
        assert_eq!(footprint.size.height, px(42.));
        assert_eq!(
            cx.debug_bounds("countdown-slot").unwrap().size.height,
            px(42.)
        );
        for id in ["home-settings", "workspace-reset"] {
            let button = cx.debug_bounds(id).unwrap();
            assert_eq!(button.size, size(px(42.), px(42.)));
            assert!(button.bottom() <= panel.bottom());
        }
        assert_eq!(
            cx.debug_bounds("Output").unwrap().size,
            size(px(28.), px(28.))
        );
        for id in [
            "Recovery",
            "deck-capture",
            "deck-utilities",
            "recorder-messages",
        ] {
            assert!(cx.debug_bounds(id).is_none(), "removed deck element: {id}");
        }
        let room = cx.debug_bounds("command-deck-breathing-room").unwrap();
        assert!(room.top() >= cx.debug_bounds("recorder-transport").unwrap().bottom());
        assert!(room.size.height >= px(28.));
        let destination = cx.debug_bounds("command-deck-output").unwrap();
        let controls = cx.debug_bounds("recorder-transport").unwrap();
        assert!(destination.top() >= controls.bottom());
        assert!(destination.bottom() <= panel.bottom());
        assert!(destination.bottom() >= panel.bottom() - px(20.));
        assert!(
            cx.debug_bounds("open-output-directory").unwrap().right()
                <= cx.debug_bounds("Output").unwrap().left()
        );
        let before = view.read_with(cx, |home, _| home.model.preferences.countdown);
        let countdown = cx.debug_bounds("home-countdown").unwrap().center();
        cx.simulate_click(countdown, Modifiers::default());
        cx.run_until_parked();
        assert_ne!(
            view.read_with(cx, |home, _| home.model.preferences.countdown),
            before
        );
        let footprint = cx.debug_bounds("record-primary-transport").unwrap();
        for (state, id) in [
            (RecorderState::Preparing, "recording-countdown"),
            (RecorderState::Recording, "recording-started-notification"),
            (RecorderState::Paused, "recording-paused-notification"),
            (RecorderState::Finalizing, "recording-saving-notification"),
        ] {
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.model.snapshot.state = state;
                    home.model.snapshot.last_artifact = Some(RecordingArtifact {
                        path: "fixture-recording.mkv".into(),
                        duration_ms: 1200.,
                        audio_tracks: 0,
                    });
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let notifications = cx.debug_bounds("command-deck-notifications").unwrap();
            let message = cx.debug_bounds(id).unwrap();
            assert!(message.top() >= notifications.top());
            assert!(
                message.bottom() <= notifications.bottom(),
                "{width} {id}: {message:?} / {notifications:?}"
            );
            assert_eq!(cx.debug_bounds("dock-panel-controls").unwrap(), panel);
            assert!(cx.debug_bounds("recording-saved-notification").is_none());
            assert_eq!(
                cx.debug_bounds("record-primary-transport").unwrap(),
                footprint
            );
        }
        cx.update(|_, cx| {
            view.update(cx, |home, cx| {
                home.model.snapshot.state = RecorderState::Idle;
                cx.notify();
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("recording-saved-notification").is_none());
        assert_eq!(cx.debug_bounds("dock-panel-controls").unwrap(), panel);
        assert_eq!(
            cx.debug_bounds("record-primary-transport").unwrap(),
            footprint
        );
        // At narrow widths the utilities wrap; every control remains fully reachable.
        let transport = cx.debug_bounds("recorder-transport").unwrap();
        for x in [-1000., 1000.] {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: transport.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(x), px(0.))),
                modifiers: Default::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            let record = cx.debug_bounds("record-primary-transport").unwrap();
            let countdown = cx.debug_bounds("countdown-slot").unwrap();
            let settings = cx.debug_bounds("home-settings").unwrap();
            let reset = cx.debug_bounds("workspace-reset").unwrap();
            assert_eq!(countdown.center().y, record.center().y);
            assert_eq!(settings.center().y, reset.center().y);
            assert!(countdown.right() <= record.left());
            if transport.size.width < px(332.) {
                assert_eq!(settings.top() - record.bottom(), px(8.));
                assert_eq!(transport.size.height, px(92.));
            } else {
                assert_eq!(settings.center().y, record.center().y);
                assert!(record.right() <= settings.left());
                assert_eq!(transport.size.height, px(42.));
            }
            assert!(settings.right() <= reset.left());
            if x < 0. {
                assert!(settings.left() >= transport.left() && reset.right() <= transport.right());
            } else {
                assert!(
                    countdown.left() >= transport.left() && record.right() <= transport.right()
                );
            }
        }
    }
}

#[gpui::test]
fn interaction_recorder_notices_do_not_reserve_footer_and_errors_remain_visible(
    cx: &mut TestAppContext,
) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.run_until_parked();
    let before = cx.debug_bounds("capture-stage").unwrap();
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.notice = Some("UI preview: development message".into());
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert_eq!(cx.debug_bounds("capture-stage").unwrap(), before);
    assert!(cx.debug_bounds("recorder-error-notification").is_none());
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.error = Some("Recording failed: output disk is full".into());
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("recorder-error-notification").is_some());
    let close = cx.debug_bounds("dismiss-error").unwrap().center();
    cx.simulate_click(close, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("recorder-error-notification").is_none());
    assert!(view.read_with(cx, |home, _| home.model.error.is_some()));
    cx.update(|window, cx| assert!(view.read(cx).focus.is_focused(window)));
    cx.update(|_, cx| view.update(cx, |_, cx| cx.notify()));
    cx.run_until_parked();
    assert!(cx.debug_bounds("recorder-error-notification").is_none());
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.error = None;
            home.model.snapshot.state = RecorderState::Failed(RecorderError::invalid(
                "Recording stopped: output unavailable",
            ));
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("recorder-error-notification").is_some());
    assert_eq!(cx.debug_bounds("capture-stage").unwrap(), before);
    let point = cx.debug_bounds("error-details").unwrap().center();
    cx.simulate_click(point, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("error-technical-details").is_some());
    let point = cx.debug_bounds("dismiss-error").unwrap().center();
    cx.simulate_click(point, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("recorder-error-notification").is_none());
    assert!(view.read_with(cx, |home, _| matches!(
        home.model.snapshot.state,
        RecorderState::Failed(_)
    )));
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.snapshot.state = RecorderState::Idle;
            cx.notify();
        })
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        view.update(cx, |home, cx| {
            home.model.error = Some("Recording failed: output disk is full".into());
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("recorder-error-notification").is_some(),
        "a later failure must open again"
    );
}

#[test]
fn theme_preferences_migrate_and_round_trip_without_losing_future_fields() {
    let mut prefs: Preferences =
        serde_json::from_str(r#"{"artwork":false,"future_setting":17}"#).unwrap();
    assert_eq!(prefs.theme, ThemeId::Black);
    assert!(!prefs.low_cost_effects);
    for theme in ThemeId::ALL {
        prefs.theme = theme;
        prefs.low_cost_effects = true;
        let saved = serde_json::to_string(&prefs).unwrap();
        let restored: Preferences = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.theme, theme);
        assert!(restored.low_cost_effects);
        assert_eq!(restored.extra_preferences["future_setting"], 17);
    }
    let future: Preferences = serde_json::from_str(r#"{"theme":"future_theme"}"#).unwrap();
    assert_eq!(future.theme, ThemeId::Black);
}

#[test]
fn surface_preference_round_trips_independently_from_theme_and_recording_settings() {
    use crate::ui::theme::SurfacePreference;
    let mut prefs: Preferences =
        serde_json::from_str(r#"{"countdown":5,"future_setting":17}"#).unwrap();
    assert_eq!(prefs.surface, SurfacePreference::Frosted);
    assert_eq!(prefs.frosted_transparency, 20);
    for theme in ThemeId::ALL {
        for surface in SurfacePreference::ALL {
            prefs.theme = theme;
            prefs.surface = surface;
            prefs.frosted_transparency = 40;
            let restored: Preferences =
                serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
            assert_eq!(restored.theme, theme);
            assert_eq!(restored.surface, surface);
            assert_eq!(restored.frosted_transparency, 40);
            assert_eq!(restored.countdown, 5);
            assert_eq!(restored.extra_preferences["future_setting"], 17);
        }
    }
    let future: Preferences = serde_json::from_str(r#"{"surface":"future_surface"}"#).unwrap();
    assert_eq!(future.surface, SurfacePreference::Frosted);
}

#[gpui::test]
fn runtime_surface_changes_keep_recording_and_queued_settings(cx: &mut TestAppContext) {
    use crate::ui::theme::{SurfacePreference, surface_treatment::is_frosted};
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            home.model.snapshot.state = RecorderState::Recording;
            home.save_latest = Some((None, home.model.preferences.clone()));
            let request_before = serde_json::to_value(&home.model.request).unwrap();
            for surface in SurfacePreference::ALL {
                home.dispatch(Action::Surface(surface), window, cx);
                for theme in ThemeId::ALL {
                    home.dispatch(Action::Theme(theme), window, cx);
                    assert_eq!(home.model.preferences.surface, surface);
                    assert_eq!(SurfacePreference::current(), surface);
                    assert_eq!(home.save_latest.as_ref().unwrap().1.surface, surface);
                    assert_eq!(home.model.snapshot.state, RecorderState::Recording);
                    assert_eq!(
                        serde_json::to_value(&home.model.request).unwrap(),
                        request_before
                    );
                }
            }
            home.dispatch(Action::Surface(SurfacePreference::Frosted), window, cx);
            home.dispatch(Action::FrostedTransparency(40), window, cx);
            assert_eq!(home.model.preferences.frosted_transparency, 40);
            assert_eq!(
                home.save_latest.as_ref().unwrap().1.frosted_transparency,
                40
            );
            assert_eq!(
                serde_json::to_value(&home.model.request).unwrap(),
                request_before
            );
            home.dispatch(Action::LowCostEffects, window, cx);
            assert!(!is_frosted());
            assert_eq!(home.model.preferences.surface, SurfacePreference::Frosted);
            home.dispatch(Action::LowCostEffects, window, cx);
            assert_eq!(
                is_frosted(),
                cfg!(any(target_os = "windows", target_os = "macos"))
            );
            home.preferences_loaded = false;
            home.dispatch(Action::Surface(SurfacePreference::Opaque), window, cx);
            assert_eq!(home.model.preferences.surface, SurfacePreference::Frosted);
        });
    });
}

#[test]
fn motion_control_text_contrast_and_critically_damped_press_settlement() {
    let _reset = ResetTheme;
    fn luminance(color: gpui::Hsla) -> f32 {
        let c: gpui::Rgba = color.into();
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }
    for theme in ThemeId::ALL {
        theme.activate();
        for bg in [
            Color::Accent,
            Color::AccentHover,
            Color::AccentPressed,
            Color::Record,
        ] {
            assert!(
                (luminance(Color::OnAccent.get()).max(luminance(bg.get())) + 0.05)
                    / (luminance(Color::OnAccent.get()).min(luminance(bg.get())) + 0.05)
                    >= 4.5,
                "{theme:?} {bg:?}"
            );
        }
        for text in [Color::PrimaryText, Color::SecondaryText, Color::MutedText] {
            assert!(
                (luminance(text.get()).max(luminance(Color::PrimarySurface.get())) + 0.05)
                    / (luminance(text.get()).min(luminance(Color::PrimarySurface.get())) + 0.05)
                    >= 4.5
            );
        }
    }
    let config = gpui::SpringConfig::new(motion::PRESS_STIFFNESS, motion::PRESS_DAMPING, 1.);
    assert!((config.canonical().1 - 1.).abs() < 0.001);
    for dt in [1. / 30., 1. / 60., 1. / 144.] {
        let mut state = gpui::SpringState {
            position: 0.,
            velocity: 0.,
        };
        for _ in 0..(0.3 / dt) as usize {
            state = config.step(state, 1., dt);
            assert!((0. ..=1.).contains(&state.position));
        }
        assert!((state.position - 1.).abs() < 0.001);
        let released = config.step(state, 0., 0.3);
        assert!(released.position.abs() < 0.001);
    }
}

#[gpui::test]
fn runtime_switch_does_not_change_recording_request_and_preserves_queued_write(
    cx: &mut TestAppContext,
) {
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|window, cx| {
        view.update(cx, |home, cx| {
            home.save_running = true; // Keep all IO queued; this is a presentation test.
            home.model.request = Some(RecordingRequest {
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
                    directory: "recordings".into(),
                    file_stem: "test".into(),
                },
            });
            home.model.snapshot.state = RecorderState::Recording;
            home.save_latest = Some((
                home.model
                    .request
                    .clone()
                    .map(frame_settings::model::SettingsDocument::new),
                home.model.preferences.clone(),
            ));
            let before = serde_json::to_value(&home.model.request).unwrap();
            for theme in ThemeId::ALL {
                home.dispatch(Action::Theme(theme), window, cx);
                assert_eq!(ThemeId::current(), theme);
                assert_eq!(home.model.preferences.theme, theme);
                assert_eq!(serde_json::to_value(&home.model.request).unwrap(), before);
                assert_eq!(home.model.snapshot.state, RecorderState::Recording);
                assert!(!home.model.command_pending);
                assert!(home.meter_task.is_none());
                assert_eq!(home.save_latest.as_ref().unwrap().1.theme, theme);
                assert!(home.save_latest.as_ref().unwrap().0.is_some());
            }
        })
    });
}

#[gpui::test]
fn motion_layout_and_idle_cost_in_idle_and_recording_states(cx: &mut TestAppContext) {
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    // No synthesized clicks, screenshots or real windows. Check the native layout
    // tree and CPU construction separately from GPU/device acceptance.
    for theme in ThemeId::ALL {
        theme.activate();
        for state in [
            RecorderState::Idle,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Finalizing,
        ] {
            for (width, height) in [(800., 640.), (1280., 850.)] {
                cx.simulate_resize(size(px(width), px(height)));
                cx.update(|_, cx| {
                    view.update(cx, |home, cx| {
                        home.model.snapshot.state = state.clone();
                        cx.notify();
                    })
                });
                cx.run_until_parked();
                for id in [
                    "record-button",
                    "source-shelf",
                    "capture-stage",
                    "audio-panel",
                    "home-settings",
                ] {
                    let b = cx
                        .debug_bounds(id)
                        .unwrap_or_else(|| panic!("{theme:?} missing {id}"));
                    assert!(
                        b.left() >= px(0.) && b.right() <= px(width),
                        "{theme:?} {state:?} {id}: {b:?}"
                    );
                    assert!(
                        b.top() >= px(0.) && b.bottom() <= px(height),
                        "{theme:?} {state:?} {id}: {b:?}"
                    );
                    assert!(b.size.width > px(0.) && b.size.height > px(0.));
                }
                {
                    let footer = cx.debug_bounds("Output").unwrap();
                    for id in ["audio-panel", "record-button"] {
                        let b = cx.debug_bounds(id).unwrap();
                        // The UI Lab dock shares one row; test 2D separation.
                        assert!(
                            b.bottom() <= footer.top()
                                || b.top() >= footer.bottom()
                                || b.right() <= footer.left()
                                || b.left() >= footer.right(),
                            "{theme:?} {width}x{height}: {id} {b:?} overlaps output {footer:?}"
                        );
                    }
                }
            }
            let before = cx.update(|_, cx| view.read(cx).render_count);
            cx.executor().advance_clock(Duration::from_secs(10));
            cx.run_until_parked();
            assert_eq!(
                cx.update(|_, cx| view.read(cx).render_count),
                before,
                "{theme:?} {state:?} idle redraw"
            );
            let start = std::time::Instant::now();
            for _ in 0..8 {
                cx.update(|_, cx| view.update(cx, |_, cx| cx.notify()));
                cx.run_until_parked();
            }
            println!(
                "headless CPU repaint {theme:?} {state:?}: {:.1} us/frame (8 frames); settled 10s: 0 owner redraws",
                start.elapsed().as_secs_f64() * 1e6 / 8.
            );
        }
    }
}

#[gpui::test]
fn normal_motion_settles_for_every_theme_and_reduced_motion_releases_press_resources(
    cx: &mut TestAppContext,
) {
    use crate::ui::primitives::motion_press::live_count;
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|_, cx| cx.set_reduce_motion(false));
    for theme in ThemeId::ALL {
        theme.activate();
        for state in [
            RecorderState::Idle,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Finalizing,
        ] {
            cx.update(|_, cx| {
                view.update(cx, |home, cx| {
                    home.model.snapshot.state = state;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let press_resources = live_count();
            for _ in 0..40 {
                // GPUI's Animation uses std::time::Instant even with test-support.
                // Advancing only the executor clock leaves optimized tests before
                // the real animation deadline; drive both clocks for this fixture.
                std::thread::sleep(Duration::from_millis(16));
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.update(|w, cx| w.simulate_next_frame(cx));
                cx.run_until_parked();
            }
            assert_eq!(
                cx.update(|w, cx| w.simulate_next_frame(cx)),
                0,
                "{theme:?} has a permanent animation callback"
            );
            assert_eq!(
                live_count(),
                press_resources,
                "Idle frames must not leak press resources"
            );
        }
    }
    cx.update(|_, cx| cx.set_reduce_motion(true));
    for theme in ThemeId::ALL {
        theme.activate();
        for _ in 0..2 {
            cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
        assert_eq!(live_count(), 0, "Reduced motion retained press resources");
    }
}

#[gpui::test]
fn shared_recorder_sections_preserve_media_transport_routes_and_preferences(
    cx: &mut TestAppContext,
) {
    let _reset = ResetTheme;
    let (view, cx) = cx.add_window_view(super::tests::home);
    cx.update(|_, cx| {
        view.update(cx, |home, _| {
            home.save_running = true;
            home.model.request = Some(RecordingRequest {
                target: CaptureTarget::Display(DisplayId("test".into())),
                fps_mode: FpsMode::AutoMatchSource,
                resolution: Resolution::Native,
                quality: Quality::High,
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
            });
        })
    });
    for (width, height) in [(800., 640.), (1280., 850.), (1920., 1080.)] {
        cx.simulate_resize(size(px(width), px(height)));
        for theme in ThemeId::ALL {
            cx.update(|w, cx| {
                view.update(cx, |home, cx| home.dispatch(Action::Theme(theme), w, cx))
            });
            cx.run_until_parked();
            let capture = cx.debug_bounds("capture-stage").unwrap();
            let record = cx.debug_bounds("record-button").unwrap();
            let audio = cx.debug_bounds("audio-panel").unwrap();
            for section in [
                RecorderSection::Video,
                RecorderSection::Camera,
                RecorderSection::Quality,
                RecorderSection::Audio,
            ] {
                let before =
                    cx.update(|_, cx| serde_json::to_value(&view.read(cx).model.request).unwrap());
                cx.update(|w, cx| {
                    view.update(cx, |home, cx| {
                        home.dispatch(Action::ToggleSection(section), w, cx)
                    })
                });
                cx.run_until_parked();
                assert_eq!(
                    cx.debug_bounds("capture-stage").unwrap(),
                    capture,
                    "{theme:?} {section:?} moved preview"
                );
                assert_eq!(
                    cx.debug_bounds("record-button").unwrap(),
                    record,
                    "{theme:?} {section:?} moved recording controls"
                );
                assert_eq!(cx.debug_bounds("audio-panel").unwrap(), audio);
                for id in ["source-settings", "camera-settings", "quality-settings"] {
                    assert!(
                        cx.debug_bounds(id).is_none(),
                        "Quick settings stay hidden: {id}"
                    );
                }
                cx.update(|w, cx| {
                    view.update(cx, |home, cx| {
                        assert_eq!(serde_json::to_value(&home.model.request).unwrap(), before);
                        assert!(!home.model.command_pending);
                        assert!(home.meter_task.is_none());
                        assert_eq!(home.model.sheet, None);
                        let restored: Preferences = serde_json::from_value(
                            serde_json::to_value(&home.model.preferences).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(
                            restored.recorder_sections,
                            home.model.preferences.recorder_sections
                        );
                        home.dispatch(Action::Theme(theme), w, cx);
                        assert_eq!(
                            home.model.preferences.recorder_sections,
                            restored.recorder_sections
                        );
                        home.dispatch(Action::ToggleSection(section), w, cx);
                    })
                });
                cx.run_until_parked();
            }
        }
    }
}
