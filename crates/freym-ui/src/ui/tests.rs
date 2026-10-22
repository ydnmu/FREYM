use super::{components::*, primitives::*};
use gpui::InputEvent;
use gpui::{Context, Modifiers, Render, TestAppContext, Window, prelude::*, px, size};

#[derive(Default)]
struct Controls {
    disabled: bool,
    clicks: usize,
    checked: bool,
    selected: bool,
    stopping: bool,
}
impl Render for Controls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Stack::vertical()
            .child(
                Button::new("button", "Action")
                    .disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                CaptureSourceCard::new("card", "Display 1")
                    .dimensions(2560, 1440, Some(165.))
                    .selected(self.selected)
                    .disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selected = true;
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                Toggle::new("toggle", "Microphone", self.checked)
                    .disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.checked = !this.checked;
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                RecordButton::new(if self.stopping {
                    RecordButtonState::Stopping
                } else {
                    RecordButtonState::Ready
                })
                .disabled(self.disabled)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clicks += 1;
                    cx.notify();
                })),
            )
    }
}

#[gpui::test]
fn composed_controls_gate_mouse_and_keyboard_activation(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|_, cx| {
        cx.set_reduce_motion(true);
        Controls::default()
    });
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    for id in ["button", "card", "toggle"] {
        let point = cx.debug_bounds(id).unwrap().center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
    }
    cx.update(|_, cx| {
        let state = view.read(cx);
        assert_eq!(state.clicks, 3);
        assert!(state.selected);
        assert!(state.checked);
    });
    // Mouse activation focuses the toggle. GPUI activates on key release;
    // Space emits the same event after the parent rerenders.
    cx.simulate_keystrokes("space");
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("space").unwrap(),
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert_eq!(view.read(cx).clicks, 4);
        assert!(!view.read(cx).checked);
        view.update(cx, |this, cx| {
            this.disabled = true;
            cx.notify();
        });
    });
    cx.run_until_parked();
    for key in ["enter", "space"] {
        cx.simulate_keystrokes(key);
        cx.update(|window, cx| {
            window.dispatch_event(
                gpui::KeyUpEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                }
                .to_platform_input(),
                cx,
            );
        });
    }
    for id in ["button", "card", "toggle", "record-button"] {
        let point = cx.debug_bounds(id).unwrap().center();
        cx.simulate_click(point, Modifiers::default());
    }
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).clicks), 4);
}

#[gpui::test]
fn finalizing_record_control_is_always_disabled(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(|_, cx| {
        cx.set_reduce_motion(true);
        Controls {
            stopping: true,
            ..Default::default()
        }
    });
    cx.run_until_parked();
    let point = cx.debug_bounds("record-button").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).clicks), 0);
}

#[derive(Default)]
struct ModalControls {
    background: usize,
    foreground: usize,
}
impl Render for ModalControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .id("modal-test")
            .relative()
            .size_full()
            .key_context("FREYM")
            .on_action(|_: &FocusNext, w, cx| w.focus_next(cx))
            .on_action(|_: &FocusPrevious, w, cx| w.focus_prev(cx))
            .child(
                Button::new("background", "Background").on_click(cx.listener(|this, _, _, cx| {
                    this.background += 1;
                    cx.notify();
                })),
            )
            .child(
                super::overlays::Modal::new("modal", "Panel")
                    .child(
                        Button::new("first", "First").on_click(cx.listener(|this, _, _, cx| {
                            this.foreground += 1;
                            cx.notify();
                        })),
                    )
                    .child(Button::new("second", "Second").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.foreground += 1;
                            cx.notify();
                        },
                    ))),
            )
    }
}

#[gpui::test]
fn modal_blocks_background_pointer_input_and_keeps_tab_navigation_inside(cx: &mut TestAppContext) {
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(|_, cx| {
        cx.set_reduce_motion(true);
        ModalControls::default()
    });
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    let point = cx.debug_bounds("background").unwrap().center();
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).background), 0);
    for key in ["tab", "tab", "shift-tab", "tab", "tab"] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
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
    }
    cx.update(|_, cx| {
        let state = view.read(cx);
        assert_eq!(state.background, 0);
        assert_eq!(state.foreground, 5);
    });
}

#[test]
fn component_configuration_and_recording_labels_are_explicit() {
    let button = Button::new("configured", "Configured")
        .variant(ButtonVariant::Danger)
        .selected(true)
        .disabled(true);
    assert!(button.is_selected());
    assert!(button.is_disabled());
    assert_eq!(RecordButtonState::Countdown.label(), "Cancel preparation");
    assert_eq!(RecordButtonState::Paused.label(), "Finish recording");
    assert!(!RecordButtonState::Stopping.active());
}

struct DismissControls {
    open: bool,
    dismissed: usize,
    background: usize,
    inside: usize,
}
impl Render for DismissControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .relative()
            .size_full()
            .key_context("FREYM")
            .child(
                Button::new("exit-background", "Background").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.background += 1;
                        cx.notify();
                    },
                )),
            )
            .when(self.open, |d| {
                d.child(
                    super::overlays::Modal::new("exit-panel", "Panel")
                        .placement(super::overlays::ModalPlacement::Side)
                        .on_dismiss(cx.listener(|this, _: &(), _, cx| {
                            this.open = false;
                            this.dismissed += 1;
                            cx.notify();
                        }))
                        .child(Button::new("exit-inside", "Inside").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.inside += 1;
                                cx.notify();
                            },
                        ))),
                )
            })
    }
}

#[gpui::test]
fn modal_exit_blocks_input_and_dismisses_once_with_reduced_motion_fallback(
    cx: &mut TestAppContext,
) {
    use super::theme::motion;
    use std::time::Duration;
    cx.update(crate::bind_keys);
    let (view, cx) = cx.add_window_view(|_, _| DismissControls {
        open: true,
        dismissed: 0,
        background: 0,
        inside: 0,
    });
    cx.simulate_resize(size(px(800.), px(640.)));
    cx.run_until_parked();
    cx.executor()
        .advance_clock(Duration::from_millis(motion::ENTRANCE_MS + 10));
    cx.update(|w, cx| w.simulate_next_frame(cx));
    cx.run_until_parked();
    let inside = cx.debug_bounds("exit-inside").unwrap().center();
    let background = cx.debug_bounds("exit-background").unwrap().center();
    let close = cx.debug_bounds("CloseSheet").unwrap().center();
    cx.simulate_click(close, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.update(|_, cx| view.read(cx).open));
    cx.simulate_click(inside, Modifiers::default());
    cx.simulate_click(background, Modifiers::default());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|_, cx| {
        let state = view.read(cx);
        assert!(state.open);
        assert_eq!((state.inside, state.background, state.dismissed), (0, 0, 0));
    });
    cx.executor()
        .advance_clock(Duration::from_millis(motion::EXIT_MS + 10));
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert_eq!(view.read(cx).dismissed, 1);
        assert!(!view.read(cx).open);
        cx.set_reduce_motion(true);
        view.update(cx, |this, cx| {
            this.open = true;
            cx.notify();
        });
    });
    cx.run_until_parked();
    let close = cx.debug_bounds("CloseSheet").unwrap().center();
    cx.simulate_click(close, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(cx.update(|_, cx| view.read(cx).dismissed), 2);
    assert!(!cx.update(|_, cx| view.read(cx).open));
}

#[test]
fn theme_scale_and_primary_control_contrast_are_consistent() {
    use super::theme::{
        colors::Color,
        radius::Radius,
        spacing::{Layout, Spacing},
    };
    let spacing = [
        Spacing::None,
        Spacing::Xxs,
        Spacing::Xs,
        Spacing::Sm,
        Spacing::Md,
        Spacing::Lg,
        Spacing::Xl,
        Spacing::Xxl,
    ]
    .map(Spacing::value);
    assert!(spacing.windows(2).all(|w| w[0] < w[1]));
    assert!(Radius::Large.value() <= Layout::CONTROL_HEIGHT / 2.);
    assert_eq!(Color::Accent.get(), Color::ActiveBorder.get());
    assert_ne!(Color::PrimaryText.get(), Color::SecondaryText.get());
    fn luminance(c: gpui::Hsla) -> f32 {
        let c: gpui::Rgba = c.into();
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }
    for background in [
        Color::Accent,
        Color::AccentHover,
        Color::AccentPressed,
        Color::Record,
        Color::RecordHover,
        Color::RecordPressed,
    ] {
        let ink = luminance(Color::OnAccent.get());
        let surface = luminance(background.get());
        assert!(
            (ink.max(surface) + 0.05) / (ink.min(surface) + 0.05) >= 4.5,
            "{background:?}"
        );
    }
}
