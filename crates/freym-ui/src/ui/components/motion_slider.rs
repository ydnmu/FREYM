//! Motion numeric editor. Writes only the existing draft input; Apply stays authoritative.
use crate::{
    input::TextInput,
    ui::theme::{colors::Color, material},
};
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, IntoElement, MouseButton,
    MouseMoveEvent, MouseUpEvent, Pixels, Render, RenderOnce, Window, canvas, div, fill, point,
    prelude::*, px, size,
};

type PercentageChange = std::rc::Rc<dyn Fn(&(u8, bool), &mut Window, &mut App)>;

pub(crate) struct MotionSlider {
    input: Entity<TextInput>,
    min: f64,
    max: f64,
    bounds: Option<Bounds<Pixels>>,
    dragging: bool,
    focus: FocusHandle,
    percentage: bool,
    disabled: bool,
    on_change: Option<PercentageChange>,
}
impl MotionSlider {
    pub fn new(input: Entity<TextInput>, min: f64, max: f64, cx: &mut Context<Self>) -> Self {
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        Self {
            input,
            min,
            max,
            bounds: None,
            dragging: false,
            focus: cx.focus_handle().tab_index(0),
            percentage: false,
            disabled: false,
            on_change: None,
        }
    }
    fn value(&self, cx: &gpui::App) -> f64 {
        self.input
            .read(cx)
            .value()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .unwrap_or(self.min)
    }
    fn set(&self, value: f64, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let value = value.clamp(self.min, self.max).round();
        let text = value.to_string();
        let changed = self.input.read(cx).value() != text;
        if changed {
            self.input.update(cx, |input, cx| input.set_value(text, cx));
        }
        if (changed || commit)
            && let Some(handler) = &self.on_change
        {
            handler(&(value as u8, commit), window, cx);
        }
    }
    fn pointer(&self, x: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bounds) = self.bounds {
            let fraction = (f32::from(x - bounds.left() - px(6.))
                / f32::from(bounds.size.width - px(12.)).max(1.))
            .clamp(0., 1.);
            self.set(
                self.min + (self.max - self.min) * fraction as f64,
                false,
                window,
                cx,
            );
        }
    }
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dragging {
            self.dragging = false;
            self.set(self.value(cx), true, window, cx);
        }
    }
}
impl Render for MotionSlider {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.value(cx);
        let fraction = ((value - self.min) / (self.max - self.min)).clamp(0., 1.) as f32;
        let view = cx.weak_entity();
        let percentage = self.percentage;
        let disabled = self.disabled;
        let id = if percentage {
            "frosted-transparency"
        } else {
            "motion-fps-slider"
        };
        div()
            .id(id)
            .debug_selector(move || id.into())
            .role(gpui::Role::Slider)
            .aria_label(if percentage {
                "Frosted transparency"
            } else {
                "Requested frames per second"
            })
            .aria_value(if percentage {
                format!("{value}%")
            } else {
                format!("{value} frames per second")
            })
            .track_focus(&self.focus)
            .tab_index(0)
            .w_full()
            // Settings rows size ordinary controls from their content. Give the
            // flexible track a real preferred width instead of only the readout.
            .when(percentage, |d| d.w(px(240.)).max_w_full())
            .min_w_0()
            .h(px(36.))
            .rounded(px(4.))
            .border_1()
            .border_color(Color::SubtleBorder.get())
            .focus_visible(|s| s.border_color(Color::ActiveBorder.get()))
            .flex()
            .items_center()
            .gap(px(8.))
            .when(percentage, |d| d.border_0().rounded(px(0.)))
            .opacity(if disabled { 0.45 } else { 1. })
            .when(!disabled, |d| d.cursor_pointer())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, w, cx| {
                    if this.disabled {
                        return;
                    }
                    w.focus(&this.focus, cx);
                    this.dragging = true;
                    this.pointer(e.position.x, w, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                if this.disabled {
                    return;
                }
                let value = this.value(cx);
                let next = match e.keystroke.key.as_str() {
                    "left" | "down" => value - 1.,
                    "right" | "up" => value + 1.,
                    "pageup" => value + 10.,
                    "pagedown" => value - 10.,
                    "home" => this.min,
                    "end" => this.max,
                    _ => return,
                };
                this.set(next, true, window, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .debug_selector(move || {
                        if percentage {
                            "frosted-transparency-track".into()
                        } else {
                            "motion-fps-track".into()
                        }
                    })
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |b, _, window, cx| {
                                let _ = view.update(cx, |this, _| this.bounds = Some(b));
                                let track = Bounds::new(
                                    point(b.left() + px(6.), b.center().y - px(1.5)),
                                    size(b.size.width - px(12.), px(3.)),
                                );
                                window.paint_quad(fill(track, Color::DefaultBorder.get()));
                                window.paint_quad(fill(
                                    Bounds::new(
                                        track.origin,
                                        size(track.size.width * fraction, px(3.)),
                                    ),
                                    material::accent(),
                                ));
                                let x = track.left() + track.size.width * fraction;
                                window.paint_quad(
                                    fill(
                                        Bounds::new(
                                            point(x - px(3.), b.center().y - px(9.)),
                                            size(px(6.), px(18.)),
                                        ),
                                        Color::PrimaryText.get(),
                                    )
                                    .corner_radii(px(2.)),
                                );
                                let movement = view.clone();
                                window.on_mouse_event(
                                    move |e: &MouseMoveEvent, phase, window, cx| {
                                        if phase.bubble() {
                                            let _ = movement.update(cx, |this, cx| {
                                                if this.dragging && e.dragging() {
                                                    this.pointer(e.position.x, window, cx);
                                                    cx.stop_propagation();
                                                }
                                            });
                                        }
                                    },
                                );
                                let release = view.clone();
                                window.on_mouse_event(
                                    move |e: &MouseUpEvent, phase, window, cx| {
                                        if phase.bubble() && e.button == MouseButton::Left {
                                            let _ = release
                                                .update(cx, |this, cx| this.finish(window, cx));
                                        }
                                    },
                                );
                            },
                        )
                        .size_full(),
                    ),
            )
            .when(percentage, |d| {
                d.child(
                    div()
                        .w(px(40.))
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(Color::SecondaryText.get())
                        .child(format!("{value}%")),
                )
            })
    }
}

/// Reuses the numeric slider's input, pointer capture and keyboard handling.
/// Preview events are separate from release/key commits to avoid writes per move.
pub(crate) fn percentage(
    value: u8,
    disabled: bool,
    handler: impl Fn(&(u8, bool), &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    PercentageSlider {
        value,
        disabled,
        handler: std::rc::Rc::new(handler),
    }
}
#[derive(IntoElement)]
struct PercentageSlider {
    value: u8,
    disabled: bool,
    handler: PercentageChange,
}
impl RenderOnce for PercentageSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let slider = window.use_keyed_state("frosted-transparency-slider", cx, |_, cx| {
            let input = cx.new(|cx| TextInput::new(self.value.min(100).to_string(), "", cx));
            let mut slider = MotionSlider::new(input, 0., 100., cx);
            slider.percentage = true;
            slider
        });
        slider.update(cx, |s, cx| {
            s.disabled = self.disabled;
            s.on_change = Some(self.handler);
            if self.disabled {
                s.dragging = false;
            }
            if !s.dragging {
                s.input.update(cx, |input, cx| {
                    input.set_value(self.value.min(100).to_string(), cx)
                });
            }
        });
        slider
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    #[gpui::test]
    fn percentage_drag_previews_once_per_value_and_commits_on_release(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        let changes = Rc::new(RefCell::new(Vec::new()));
        let events = changes.clone();
        let (view, cx) = cx.add_window_view(|_, cx| {
            let input = cx.new(|cx| TextInput::new("20", "", cx));
            let mut slider = MotionSlider::new(input, 0., 100., cx);
            slider.percentage = true;
            slider.on_change = Some(Rc::new(move |event, _, _| events.borrow_mut().push(*event)));
            slider
        });
        cx.update(|window, cx| {
            view.update(cx, |slider, cx| {
                slider.bounds = Some(Bounds::new(point(px(0.), px(0.)), size(px(112.), px(36.))));
                slider.dragging = true;
                for x in [56., 56., 200.] {
                    slider.pointer(px(x), window, cx);
                }
                assert_eq!(*changes.borrow(), [(50, false), (100, false)]);
                slider.finish(window, cx);
                slider.finish(window, cx);
                assert_eq!(*changes.borrow(), [(50, false), (100, false), (100, true)]);
            })
        });
    }
    #[gpui::test]
    fn slider_clamps_drag_and_writes_only_the_draft_input(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let input = cx.new(|cx| TextInput::new("59.94", "", cx));
            MotionSlider::new(input, 1., 240., cx)
        });
        cx.update(|window, cx| {
            view.update(cx, |slider, cx| {
                assert_eq!(slider.value(cx), 59.94);
                slider.bounds = Some(Bounds::new(point(px(0.), px(0.)), size(px(112.), px(36.))));
                for (x, expected) in [(-30., "1"), (56., "121"), (200., "240")] {
                    slider.pointer(px(x), window, cx);
                    assert_eq!(slider.input.read(cx).value(), expected);
                }
            })
        });
    }
}
