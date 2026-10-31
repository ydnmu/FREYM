//! Independent meter entity; real PCM targets, no global redraw or idle frame loop.
use crate::ui::theme::{colors::Color, motion, spacing::Layout};
use gpui::{Context, Render, Window, canvas, div, fill, point, prelude::*, px, size};
use std::time::Instant;

pub struct AudioMeter {
    value: f32,
    target: f32,
    peak: f32,
    last: Instant,
    frame_pending: bool,
}
impl Default for AudioMeter {
    fn default() -> Self {
        Self {
            value: 0.,
            target: 0.,
            peak: 0.,
            last: Instant::now(),
            frame_pending: false,
        }
    }
}
impl AudioMeter {
    pub fn sample(&mut self, rms: Option<f32>, peak: Option<f32>, cx: &mut Context<Self>) {
        fn normalized(v: Option<f32>) -> f32 {
            v.filter(|x| x.is_finite())
                .map(|x| ((20. * x.max(0.000001).log10() + 60.) / 60.).clamp(0., 1.))
                .unwrap_or(0.)
        }
        let next = normalized(rms);
        let peak = normalized(peak);
        if (next - self.target).abs() > 0.002 || (peak - self.peak).abs() > 0.002 {
            self.target = next;
            self.peak = peak;
            cx.notify();
        }
    }
}
impl Render for AudioMeter {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.25);
        self.last = now;
        if cx.reduce_motion() {
            self.value = self.target;
        } else {
            let tau = if self.target > self.value {
                motion::METER_ATTACK_SECONDS
            } else {
                motion::METER_RELEASE_SECONDS
            };
            self.value += (self.target - self.value) * (1. - (-dt / tau).exp());
        }
        if (self.value - self.target).abs() > 0.002 && !self.frame_pending {
            self.frame_pending = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.frame_pending = false;
                cx.notify();
            });
        } else if (self.value - self.target).abs() <= 0.002 {
            self.value = self.target;
        }
        let value = self.value;
        let peak = self.peak;
        div()
            .w(px(Layout::METER_WIDTH))
            .h(px(Layout::METER_HEIGHT))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        for i in 0..16 {
                            let height = 4. + (i as f32 * 0.75).min(12.);
                            let threshold = i as f32 / 16.;
                            let color = if threshold < value {
                                Color::Accent.get()
                            } else {
                                Color::MutedText.get().opacity(0.22)
                            };
                            window.paint_quad(fill(
                                gpui::Bounds {
                                    origin: point(
                                        bounds.left() + px(i as f32 * 6.5),
                                        bounds.bottom() - px(height),
                                    ),
                                    size: size(px(3.), px(height)),
                                },
                                color,
                            ));
                        }
                        if peak > 0.05 {
                            window.paint_quad(fill(
                                gpui::Bounds {
                                    origin: point(bounds.left() + px(peak * 100.), bounds.top()),
                                    size: size(px(2.), px(2.)),
                                },
                                if peak > 0.97 {
                                    Color::Danger.get()
                                } else {
                                    Color::Accent.get()
                                },
                            ));
                        }
                    },
                )
                .size_full(),
            )
    }
}
