//! Headless GPUI checks; no desktop automation, capture, or manual visual acceptance.
use super::*;
use crate::ui::theme::{ThemeId, colors::Color, motion};
use gpui::{TestAppContext, size};

struct ResetTheme;
impl Drop for ResetTheme {
    fn drop(&mut self) {
        ThemeId::ClassicLight.activate();
        crate::ui::theme::set_low_cost_effects(false);
    }
}

#[test]
fn theme_preferences_migrate_and_round_trip_without_losing_future_fields() {
    let mut prefs: Preferences =
        serde_json::from_str(r#"{"artwork":false,"future_setting":17}"#).unwrap();
    assert_eq!(prefs.theme, ThemeId::AmoledBlack);
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
    assert_eq!(future.theme, ThemeId::ClassicLight);
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
    for theme in [ThemeId::VioletMotion, ThemeId::EmberMotion] {
        theme.activate();
        for bg in [
            Color::Accent,
            Color::AccentHover,
            Color::AccentPressed,
            Color::Record,
        ] {
            assert!(
                (luminance(Color::OnAccent.get()) + 0.05) / (luminance(bg.get()) + 0.05) >= 4.5,
                "{theme:?} {bg:?}"
            );
        }
        for text in [Color::PrimaryText, Color::SecondaryText, Color::MutedText] {
            assert!(
                (luminance(text.get()) + 0.05) / (luminance(Color::PrimarySurface.get()) + 0.05)
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
                    "source-settings",
                    "camera-settings",
                    "quality-settings",
                    "more-options",
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
            assert!(live_count() > 0);
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
                    let b = cx.debug_bounds(id).unwrap();
                    assert!(
                        b.left() >= px(0.) && b.right() <= px(width) && b.bottom() <= px(height),
                        "{theme:?} {section:?} {id} {b:?}"
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
