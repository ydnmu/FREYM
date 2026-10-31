use crate::ui::{
    primitives::{Button, ButtonSize, ButtonVariant, clickable},
    theme::{colors::Color, motion},
};
use gpui::{AnimationExt, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordButtonState {
    Ready,
    Countdown,
    Recording,
    Paused,
    Stopping,
    Failed,
}
impl RecordButtonState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Record",
            Self::Countdown => "Cancel preparation",
            Self::Recording => "Stop recording",
            Self::Paused => "Finish recording",
            Self::Stopping => "Finalizing recording…",
            Self::Failed => "Reset recorder",
        }
    }
    pub fn active(self) -> bool {
        matches!(self, Self::Countdown | Self::Recording | Self::Paused)
    }
}
#[derive(IntoElement)]
pub struct RecordButton {
    button: Button,
    state: RecordButtonState,
    label: SharedString,
    transport: Option<super::record_controls::TransportMotion>,
}
impl RecordButton {
    pub fn new(state: RecordButtonState) -> Self {
        Self {
            button: Button::new("record-button", state.label())
                .size(ButtonSize::Record)
                .variant(ButtonVariant::Primary),
            state,
            label: state.label().into(),
            transport: None,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self.button = self.button.label(self.label.clone());
        self
    }
    pub(super) fn transport(mut self, motion: super::record_controls::TransportMotion) -> Self {
        self.transport = Some(motion);
        self
    }
    pub(super) fn is_paused(&self) -> bool {
        self.state == RecordButtonState::Paused
    }
    pub(super) fn compact(mut self) -> Self {
        self.button = self.button.size(ButtonSize::RecordCompact).fill_width();
        self
    }
}
clickable!(RecordButton);
impl RenderOnce for RecordButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let indicator = super::record_indicator::RecordIndicator {
            state: self.state,
            phase: 0.,
        };
        let marker = indicator.into_any_element();
        let disabled = self.button.is_disabled() || self.state == RecordButtonState::Stopping;
        let button = if self.state == RecordButtonState::Recording {
            self.button.variant(ButtonVariant::Record)
        } else if matches!(
            self.state,
            RecordButtonState::Countdown
                | RecordButtonState::Paused
                | RecordButtonState::Stopping
                | RecordButtonState::Failed
        ) {
            self.button.variant(ButtonVariant::Secondary)
        } else {
            self.button
        };
        if let Some(transport) = self.transport {
            // The visual faces change independently; the Button keeps its real action label.
            let label = match self.label.as_ref() {
                "Please wait…" | "Closing safely…" => self.label,
                "Start recording" | "Record" => "Record".into(),
                "Cancel preparation" => "Cancel".into(),
                "Stop replay buffer" => "Stop replay".into(),
                "Stopping replay…" => "Stopping…".into(),
                _ if transport.target => "Record".into(),
                _ => self.label,
            };
            let progress = transport.progress;
            let start = div()
                .id("record-start-face")
                .debug_selector(|| "record-start-face".into())
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .whitespace_nowrap()
                .gap(px(10.))
                .opacity((1. - progress / 0.7).clamp(0., 1.))
                .left(px(-3. * progress))
                .child(marker)
                .child(label);
            let square = div()
                .rounded(px(3.))
                .size(px(15. * (0.92 + 0.08 * progress)))
                .bg(if self.state == RecordButtonState::Recording {
                    Color::OnAccent.get()
                } else {
                    Color::PrimaryText.get()
                });
            let stop = div()
                .id("record-stop-face")
                .debug_selector(|| "record-stop-face".into())
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .opacity(((progress - 0.35) / 0.55).clamp(0., 1.))
                .child(square);
            let face = button
                .content(
                    div()
                        .relative()
                        .size_full()
                        .overflow_hidden()
                        .child(start)
                        .child(stop),
                )
                .disabled(disabled)
                .into_any_element();
            return record_frame(face, disabled, window, cx);
        }
        let face = button
            .content(marker)
            .content(div().child(self.label).with_animation(
                format!("record-state-{:?}", self.state),
                motion::state_change(),
                |d, t| {
                    d.relative()
                        .top(px((1. - t) * motion::STATE_TRAVEL))
                        .opacity(t)
                },
            ))
            .disabled(disabled)
            .into_any_element();
        record_frame(face, disabled, window, cx)
    }
}

#[derive(Default)]
struct HoverLight {
    started: Option<std::time::Instant>,
    disabled: bool,
    hovered: bool,
    white: bool,
}
impl HoverLight {
    fn set_disabled(&mut self, disabled: bool) -> bool {
        if self.disabled == disabled {
            return false;
        }
        self.disabled = disabled;
        self.started = (self.hovered && !disabled).then(std::time::Instant::now);
        true
    }
    fn set_hovered(&mut self, hovered: bool) -> bool {
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.started = (hovered && !self.disabled).then(std::time::Instant::now);
        true
    }
}
impl gpui::Render for HoverLight {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // This entity owns only the decoration. Animation frames do not invalidate Home.
        let started = self.started.filter(|_| !self.disabled && self.hovered);
        let reduced = cx.reduce_motion();
        let white = self.white;
        if started.is_some() && !reduced && window.is_visible() {
            let light = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                if window.is_visible() && !cx.reduce_motion() {
                    let _ = light.update(cx, |light, cx| {
                        // A queued frame after exit/disable must not restart the loop.
                        if light.hovered && !light.disabled && light.started.is_some() {
                            cx.notify();
                        }
                    });
                }
            });
        }
        div()
            .relative()
            .size_full()
            .when_some(started, |d, started| {
                d.child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, w, _| {
                            // Sample at paint time, preserve subpixel paths, and keep all work
                            // bounded. No idle canvas, timer, blur or per-frame UI layout changes.
                            let bounds = bounds.dilate(-px(8.)).dilate(-px(0.8));
                            if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                                return;
                            }
                            let perimeter = perimeter_length(bounds);
                            let elapsed = started.elapsed().as_secs_f64();
                            // Constant logical pixels/second, independent of button width.
                            // f64 modulo precedes conversion so long hovers retain precision.
                            let phase = if reduced {
                                0.18
                            } else {
                                ((elapsed * 180.) % f64::from(perimeter)) as f32 / perimeter
                            };
                            let fade = if reduced {
                                1.
                            } else {
                                (elapsed as f32 / 0.14).clamp(0., 1.)
                            };
                            let length = (perimeter * 0.48).clamp(96., 240.).min(perimeter * 0.60);
                            // Native equivalent of a moving gradient masked to a rounded border.
                            // Arc-length sampling keeps the gradient and its white core continuous
                            // through every corner; nested bands approximate a soft radial bloom.
                            let samples = (length / 2.).ceil().clamp(24., 96.) as usize;
                            for (width, opacity) in [
                                (7., 0.035),
                                (4.5, 0.08),
                                (2.8, 0.18),
                                (1.25, 0.95),
                                (0.5, 1.),
                            ] {
                                for i in 0..samples {
                                    let a = i as f32 / samples as f32;
                                    let b = (i + 1) as f32 / samples as f32;
                                    if width < 0.6 && b < 0.76 {
                                        continue;
                                    }
                                    if let Some(path) =
                                        beam_strip(bounds, phase, length, width, a, b)
                                    {
                                        let color = |t: f32| {
                                            let core = if width <= 1.25 {
                                                ((t - 0.72) / 0.24).clamp(0., 1.)
                                            } else {
                                                0.
                                            };
                                            let cap = ((1. - t) / 0.035).clamp(0., 1.);
                                            let alpha = t * t * cap * opacity * fade;
                                            gpui::Rgba {
                                                r: 1.,
                                                g: if white { 1. } else { 0.06 + core * 0.90 },
                                                b: if white { 1. } else { 0.10 + core * 0.86 },
                                                a: alpha,
                                            }
                                        };
                                        let (p, _) = perimeter_sample(
                                            bounds,
                                            phase - length / perimeter * (1. - a),
                                        );
                                        let (q, _) = perimeter_sample(
                                            bounds,
                                            phase - length / perimeter * (1. - b),
                                        );
                                        let angle = f32::from(q.y - p.y)
                                            .atan2(f32::from(q.x - p.x))
                                            .to_degrees()
                                            + 90.;
                                        w.paint_path(
                                            path,
                                            gpui::linear_gradient(
                                                angle,
                                                gpui::linear_color_stop(color(a), 0.),
                                                gpui::linear_color_stop(color(b), 1.),
                                            ),
                                        );
                                    }
                                }
                            }
                        },
                    )
                    .absolute()
                    .inset(-px(8.)),
                )
            })
    }
}
fn perimeter_length(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
    let w = f32::from(bounds.size.width).max(2.);
    let h = f32::from(bounds.size.height).max(2.);
    let r = 8_f32.min(w / 2.).min(h / 2.);
    2. * (w + h - 4. * r) + std::f32::consts::TAU * r
}
fn beam_strip(
    bounds: gpui::Bounds<gpui::Pixels>,
    phase: f32,
    length: f32,
    width: f32,
    a: f32,
    b: f32,
) -> Option<gpui::Path<gpui::Pixels>> {
    let perimeter = perimeter_length(bounds);
    let mut path = gpui::PathBuilder::fill();
    let vertex = |t: f32, side: f32| {
        let (point, normal) = perimeter_sample(bounds, phase - length / perimeter * (1. - t));
        point + gpui::point(px(normal.0 * width * side), px(normal.1 * width * side))
    };
    path.move_to(vertex(a, 1.));
    path.line_to(vertex((a + b) / 2., 1.));
    path.line_to(vertex(b, 1.));
    path.line_to(vertex(b, -1.));
    path.line_to(vertex((a + b) / 2., -1.));
    path.line_to(vertex(a, -1.));
    path.close();
    path.build().ok()
}
fn perimeter_sample(
    bounds: gpui::Bounds<gpui::Pixels>,
    progress: f32,
) -> (gpui::Point<gpui::Pixels>, (f32, f32)) {
    let w = f32::from(bounds.size.width).max(2.);
    let h = f32::from(bounds.size.height).max(2.);
    let r = 8_f32.min(w / 2.).min(h / 2.);
    let arc = std::f32::consts::FRAC_PI_2 * r;
    let horizontal = (w - 2. * r).max(0.);
    let vertical = (h - 2. * r).max(0.);
    let lengths = [
        horizontal, arc, vertical, arc, horizontal, arc, vertical, arc,
    ];
    let mut d = progress.rem_euclid(1.) * lengths.iter().sum::<f32>();
    let mut segment = 7;
    for (i, len) in lengths.iter().enumerate() {
        if d <= *len {
            segment = i;
            break;
        }
        d -= len;
    }
    let (x, y, nx, ny) = match segment {
        0 => (r + d, 0., 0., -1.),
        2 => (w, r + d, 1., 0.),
        4 => (w - r - d, h, 0., 1.),
        6 => (0., h - r - d, -1., 0.),
        _ => {
            let (cx, cy, start) = match segment {
                1 => (w - r, r, -std::f32::consts::FRAC_PI_2),
                3 => (w - r, h - r, 0.),
                5 => (r, h - r, std::f32::consts::FRAC_PI_2),
                _ => (r, r, std::f32::consts::PI),
            };
            let a = start + d / r.max(1.);
            (cx + r * a.cos(), cy + r * a.sin(), a.cos(), a.sin())
        }
    };
    (
        gpui::point(bounds.left() + px(x), bounds.top() + px(y)),
        (nx, ny),
    )
}
fn record_frame(
    face: gpui::AnyElement,
    disabled: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    hover_frame("record-border-beam", face, disabled, false, window, cx)
}
pub(crate) fn hover_frame(
    id: &'static str,
    face: gpui::AnyElement,
    disabled: bool,
    white: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    // Observe the holder, not the animated entity: its frame notifications stay local.
    let holder = window.use_keyed_state(format!("hover-light-{id}"), cx, |_, cx| {
        cx.new(|_| HoverLight::default())
    });
    let light = holder.read(cx).clone();
    light.update(cx, |light, cx| {
        light.white = white;
        if light.set_disabled(disabled) {
            cx.notify();
        }
    });
    let hover = light.clone();
    div()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .size_full()
        .on_hover(move |over, _, cx| {
            hover.update(cx, |s, cx| {
                if s.set_hovered(*over) {
                    cx.notify();
                }
            });
        })
        .child(face)
        .child(div().absolute().inset_0().size_full().child(light))
        .into_any_element()
}
#[cfg(test)]
mod hover_tests {
    use super::*;
    #[gpui::test]
    fn remaining_beam_redraws_only_during_hover_and_respects_reduced_motion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(false);
            HoverLight::default()
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        view.update(cx, |light, cx| {
            light.set_hovered(true);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.update(|w, cx| w.simulate_next_frame(cx)) > 0);
        view.update(cx, |light, cx| {
            light.set_hovered(false);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        view.update(cx, |light, cx| {
            cx.set_reduce_motion(true);
            light.set_hovered(true);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
    }
    #[test]
    fn hover_light_stops_at_exit_and_tracks_disabled_pointer_without_restarting() {
        let mut light = HoverLight::default();
        assert!(light.started.is_none());
        assert!(light.set_hovered(true));
        let entered = light.started;
        assert!(!light.set_hovered(true));
        assert_eq!(light.started, entered);
        assert!(light.set_hovered(false));
        assert!(light.started.is_none());
        assert!(light.set_disabled(true));
        assert!(light.set_hovered(true));
        assert!(light.started.is_none());
        assert!(light.set_disabled(false));
        assert!(light.started.is_some());
        assert!(light.set_hovered(false));
        assert!(light.started.is_none());
    }
    #[test]
    fn correct_record_light_stays_on_rounded_button_perimeter() {
        let b = gpui::Bounds::new(gpui::point(px(10.), px(20.)), gpui::size(px(180.), px(40.)));
        for i in 0..200 {
            let (p, _) = perimeter_sample(b, i as f32 / 199.);
            assert!(
                p.x >= b.left() - px(0.01)
                    && p.x <= b.right() + px(0.01)
                    && p.y >= b.top() - px(0.01)
                    && p.y <= b.bottom() + px(0.01)
            );
        }
    }
    #[test]
    fn hover_light_has_continuous_speed_and_normals_across_corners_and_loop_seam() {
        let b = gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(180.), px(42.)));
        let perimeter = perimeter_length(b);
        let step = 0.1;
        for i in 0..2000 {
            let phase = i as f32 / 1999.;
            let (a, n) = perimeter_sample(b, phase);
            let (next, _) = perimeter_sample(b, phase + step / perimeter);
            let dx = f32::from(next.x - a.x);
            let dy = f32::from(next.y - a.y);
            assert!(((dx * dx + dy * dy).sqrt() - step).abs() < 0.001);
            assert!((n.0 * n.0 + n.1 * n.1 - 1.).abs() < 0.0001);
        }
        assert_eq!(perimeter_sample(b, 0.), perimeter_sample(b, 1.));
    }
}
