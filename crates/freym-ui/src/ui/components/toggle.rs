use crate::ui::{
    primitives::{Button, ButtonSize, clickable, motion_surface},
    theme::{motion, spacing::Layout},
};
use gpui::{
    AnimationExt, App, ElementId, IntoElement, Refineable, RenderOnce, SharedString, Window, div,
    prelude::*, px,
};
#[derive(IntoElement)]
pub struct Toggle {
    button: Button,
    checked: bool,
}
impl Toggle {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, checked: bool) -> Self {
        Self {
            button: Button::new(id, label)
                .variant(crate::ui::primitives::ButtonVariant::Ghost)
                .size(ButtonSize::Compact),
            checked,
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub(crate) fn indicator(checked: bool) -> impl IntoElement {
        SwitchIndicator {
            checked,
            small: false,
        }
    }
    pub(crate) fn small_indicator(checked: bool) -> impl IntoElement {
        SwitchIndicator {
            checked,
            small: true,
        }
    }
}
#[derive(IntoElement)]
struct SwitchIndicator {
    checked: bool,
    small: bool,
}
impl RenderOnce for SwitchIndicator {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let small = self.small;
        let (width, height, thumb_size) = if small {
            (
                Layout::TOGGLE_SMALL_WIDTH,
                Layout::TOGGLE_SMALL_HEIGHT,
                Layout::TOGGLE_SMALL_THUMB,
            )
        } else {
            (
                Layout::TOGGLE_WIDTH,
                Layout::TOGGLE_HEIGHT,
                Layout::TOGGLE_THUMB,
            )
        };
        let milliseconds = motion::SELECTION_MS as f32;

        let animation = motion::switch_selection(if checked { 1. } else { 0. }, milliseconds);

        let thumb: Option<gpui::Background> = None;

        let background: Option<gpui::Background> = None;

        let track = div()
            .w(px(width))
            .h(px(height))
            .flex_shrink_0()
            .relative()
            .overflow_hidden()
            .rounded_full()
            .with_spring("switch-position", animation, move |d, t| {
                let track_face = if let Some(b) = background {
                    b
                } else {
                    crate::ui::theme::controls::switch_face_at(t, false)
                };
                d.bg(track_face)
                    .when(!small, |d| {
                        d.shadow(crate::ui::theme::controls::switch_shadows_at(t, false))
                    })
                    .child({
                        let thumb_face = if let Some(b) = thumb {
                            b
                        } else {
                            crate::ui::theme::controls::switch_face_at(t, true)
                        };
                        let mut face = div()
                            .debug_selector(|| "switch-thumb".into())
                            .absolute()
                            .size(px(thumb_size))
                            .top(px(Layout::TOGGLE_INSET))
                            .rounded_full()
                            .bg(thumb_face)
                            .when(!small, |d| {
                                d.shadow(crate::ui::theme::controls::switch_shadows_at(t, true))
                            })
                            .left(px(Layout::TOGGLE_INSET));
                        let mut style = gpui::Style::default();
                        style.refine(face.style());
                        face.bg(gpui::rgba(0)).shadow(vec![]).child(
                            gpui::canvas(
                                |_, _, _| {},
                                move |mut bounds, _, window, _| {
                                    bounds.origin.x +=
                                        px((width - thumb_size - Layout::TOGGLE_INSET * 2.)
                                            * t.clamp(0., 1.));
                                    motion_surface::paint("switch-thumb", bounds, &style, window);
                                },
                            )
                            .size_full(),
                        )
                    })
            });
        let element = div()
            .w(px(width))
            .h(px(height))
            .flex_shrink_0()
            .child(track);

        element.into_any_element()
    }
}
clickable!(Toggle);
impl RenderOnce for Toggle {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.button
            .toggled(self.checked)
            .content(Self::indicator(self.checked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext};
    use std::time::{Duration, Instant};

    struct Fixture {
        checked: bool,
        disabled: bool,
    }
    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            Toggle::new("stable-switch", "Switch", self.checked)
                .disabled(self.disabled)
                .on_click(cx.listener(|s, _, _, cx| {
                    s.checked = !s.checked;
                    cx.notify();
                }))
        }
    }
    #[gpui::test]
    #[ignore = "bounded manual animation probe"]
    fn switch_settlement_probe(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(false);
            Fixture {
                checked: false,
                disabled: false,
            }
        });
        cx.run_until_parked();
        let off = motion_surface::test_bounds("switch-thumb").left();
        for checked in [true, false] {
            let start = Instant::now();
            cx.update(|_, cx| {
                view.update(cx, |s, cx| {
                    s.checked = checked;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let mut frames = 0;
            loop {
                std::thread::sleep(Duration::from_millis(8));
                let callbacks = cx.update(|w, cx| w.simulate_next_frame(cx));
                cx.run_until_parked();
                if callbacks == 0 {
                    break;
                }
                frames += 1;
                assert!(start.elapsed() < Duration::from_secs(1));
            }
            println!(
                "switch checked={checked}: settles {:.1} ms, {frames} requested frames, left={:?}, off={off:?}",
                start.elapsed().as_secs_f64() * 1000.,
                motion_surface::test_bounds("switch-thumb").left()
            );
        }
    }

    #[gpui::test]
    fn switch_disabled_does_not_reset_travel(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(false);
            Fixture {
                checked: false,
                disabled: false,
            }
        });
        cx.run_until_parked();
        let off = motion_surface::test_bounds("switch-thumb").left();
        cx.update(|_, cx| {
            view.update(cx, |s, cx| {
                s.checked = true;
                cx.notify();
            })
        });
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(40));
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        let moving = motion_surface::test_bounds("switch-thumb").left();
        assert!(moving > off);
        cx.update(|_, cx| {
            view.update(cx, |s, cx| {
                s.disabled = true;
                cx.notify();
            })
        });
        cx.run_until_parked();
        let disabled = motion_surface::test_bounds("switch-thumb").left();
        assert!(
            disabled < off + px(15.5),
            "disabling reset in-flight motion to its endpoint: {moving:?} -> {disabled:?}"
        );
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.update(|w, _| w.refresh());
        cx.run_until_parked();
        assert_eq!(
            motion_surface::test_bounds("switch-thumb").left(),
            off + px(16.)
        );
    }

    #[gpui::test]
    fn switch_rapid_reversals_keep_position_and_stop_requesting_frames(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(false);
            Fixture {
                checked: false,
                disabled: false,
            }
        });
        cx.run_until_parked();
        let off = motion_surface::test_bounds("switch-thumb").left();
        for checked in [true, false, true, false] {
            let before = motion_surface::test_bounds("switch-thumb").left();
            cx.update(|_, cx| {
                view.update(cx, |s, cx| {
                    s.checked = checked;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let after = motion_surface::test_bounds("switch-thumb").left();
            assert!(
                (after - before).abs() <= px(1.),
                "reversal jumped: {before:?} -> {after:?}"
            );
            std::thread::sleep(Duration::from_millis(24));
            cx.update(|w, cx| w.simulate_next_frame(cx));
            cx.run_until_parked();
            let moving = motion_surface::test_bounds("switch-thumb").left();
            assert!(
                moving > off && moving < off + px(16.),
                "rapid retarget snapped to an endpoint"
            );
        }
        std::thread::sleep(Duration::from_millis(180));
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        assert_eq!(motion_surface::test_bounds("switch-thumb").left(), off);
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
    }

    #[gpui::test]
    fn switch_initial_checked_disabled_and_reduced_motion(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(true);
            Fixture {
                checked: true,
                disabled: true,
            }
        });
        cx.run_until_parked();
        let on = motion_surface::test_bounds("switch-thumb").left();
        let hit = cx.debug_bounds("stable-switch").unwrap().center();
        cx.simulate_click(hit, Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| view.read(cx).checked),
            "disabled switch activated"
        );
        assert_eq!(motion_surface::test_bounds("switch-thumb").left(), on);
        cx.update(|_, cx| {
            view.update(cx, |s, cx| {
                s.disabled = false;
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.simulate_click(hit, Modifiers::default());
        cx.run_until_parked();
        assert!(!cx.update(|_, cx| view.read(cx).checked));
        assert_eq!(
            motion_surface::test_bounds("switch-thumb").left(),
            on - px(16.)
        );
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
    }
}
