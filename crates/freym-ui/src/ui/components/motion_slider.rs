//! Motion numeric editor. Writes only the existing draft input; Apply stays authoritative.
use crate::{
    input::TextInput,
    ui::theme::{colors::Color, material},
};
use gpui::{
    Bounds, Context, Entity, FocusHandle, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels,
    Render, Window, canvas, div, fill, point, prelude::*, px, size,
};

pub(crate) struct MotionSlider {
    input: Entity<TextInput>,
    min: f64,
    max: f64,
    bounds: Option<Bounds<Pixels>>,
    dragging: bool,
    focus: FocusHandle,
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
    fn set(&self, value: f64, cx: &mut Context<Self>) {
        let value = value.clamp(self.min, self.max).round().to_string();
        self.input
            .update(cx, |input, cx| input.set_value(value, cx));
    }
    fn pointer(&self, x: Pixels, cx: &mut Context<Self>) {
        if let Some(bounds) = self.bounds {
            let fraction = (f32::from(x - bounds.left() - px(6.))
                / f32::from(bounds.size.width - px(12.)).max(1.))
            .clamp(0., 1.);
            self.set(self.min + (self.max - self.min) * fraction as f64, cx);
        }
    }
}
impl Render for MotionSlider {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.value(cx);
        let fraction = ((value - self.min) / (self.max - self.min)).clamp(0., 1.) as f32;
        let view = cx.weak_entity();
        div()
            .id("motion-fps-slider")
            .debug_selector(|| "motion-fps-slider".into())
            .role(gpui::Role::Slider)
            .aria_label("Requested frames per second")
            .aria_value(format!("{value} frames per second"))
            .track_focus(&self.focus)
            .tab_index(0)
            .w_full()
            .h(px(36.))
            .rounded(px(4.))
            .border_1()
            .border_color(Color::SubtleBorder.get())
            .focus_visible(|s| s.border_color(Color::ActiveBorder.get()))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, w, cx| {
                    w.focus(&this.focus, cx);
                    this.dragging = true;
                    this.pointer(e.position.x, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
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
                this.set(next, cx);
                cx.stop_propagation();
            }))
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
                            Bounds::new(track.origin, size(track.size.width * fraction, px(3.))),
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
                        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                            if phase.bubble() {
                                let _ = movement.update(cx, |this, cx| {
                                    if this.dragging && e.dragging() {
                                        this.pointer(e.position.x, cx);
                                        cx.stop_propagation();
                                    }
                                });
                            }
                        });
                        let release = view.clone();
                        window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                            if phase.bubble() && e.button == MouseButton::Left {
                                let _ = release.update(cx, |this, _| this.dragging = false);
                            }
                        });
                    },
                )
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    #[gpui::test]
    fn slider_clamps_drag_and_writes_only_the_draft_input(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let input = cx.new(|cx| TextInput::new("59.94", "", cx));
            MotionSlider::new(input, 1., 240., cx)
        });
        cx.update(|_, cx| {
            view.update(cx, |slider, cx| {
                assert_eq!(slider.value(cx), 59.94);
                slider.bounds = Some(Bounds::new(point(px(0.), px(0.)), size(px(112.), px(36.))));
                for (x, expected) in [(-30., "1"), (56., "121"), (200., "240")] {
                    slider.pointer(px(x), cx);
                    assert_eq!(slider.input.read(cx).value(), expected);
                }
            })
        });
    }
}
