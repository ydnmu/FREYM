//! Small activity grids. One cached native view paints all nine cells.
use gpui::{
    Bounds, Context, Hsla, IntoElement, Render, Task, Window, canvas, div, fill, point, prelude::*,
    px, size,
};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(33);
const PAINT_LEASE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityStyle {
    Tiles,
    Dots,
}

fn brightness(phase: f32, row: usize, column: usize) -> f32 {
    let delay = ((2 - row) as f32 + (column as f32 - 1.).abs()) * 0.2;
    let crest = 0.5 + 0.5 * ((phase - delay) * std::f32::consts::TAU).cos();
    0.12 + 0.88 * crest.powi(4)
}

/// Presentation only. The host owns the active state and any loading semantics.
/// Reference: Zeron's compact activity grids; independently implemented wave.
pub struct ActivityIndicator {
    style: ActivityStyle,
    colors: [Hsla; 3],
    size: f32,
    active: bool,
    visible: bool,
    period: Duration,
    phase: f32,
    sampled_at: Instant,
    was_running: bool,
    painted_at: Rc<Cell<Instant>>,
    clock: Option<Task<()>>,
}
impl ActivityIndicator {
    pub fn new(style: ActivityStyle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_visibility(window, |this, visibility, _, cx| {
            this.visible = visibility.is_visible();
            if !this.visible {
                this.advance(Instant::now(), false);
                this.clock = None;
            }
            cx.notify();
        })
        .detach();
        Self {
            style,
            colors: [0xb6d3ef, 0xedb185, 0xf888a0].map(|color| gpui::rgb(color).into()),
            size: 48.,
            active: true,
            visible: window.is_visible(),
            period: Duration::from_millis(750),
            phase: 0.,
            sampled_at: Instant::now(),
            was_running: false,
            painted_at: Rc::new(Cell::new(Instant::now())),
            clock: None,
        }
    }
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.advance(Instant::now(), false);
            self.active = active;
            if !active {
                self.clock = None;
            }
            cx.notify();
        }
    }
    pub fn set_period(&mut self, period: Duration, cx: &mut Context<Self>) {
        let period = period.clamp(Duration::from_millis(250), Duration::from_secs(5));
        if self.period != period {
            self.advance(Instant::now(), self.was_running);
            self.period = period;
            cx.notify();
        }
    }
    pub fn set_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if size.is_finite() && self.size != size.clamp(16., 96.) {
            self.size = size.clamp(16., 96.);
            cx.notify();
        }
    }
    pub fn set_colors(&mut self, colors: [Hsla; 3], cx: &mut Context<Self>) {
        if self.colors != colors {
            self.colors = colors;
            cx.notify();
        }
    }
    fn advance(&mut self, now: Instant, running: bool) {
        if self.was_running {
            self.phase = (self.phase
                + now.saturating_duration_since(self.sampled_at).as_secs_f32()
                    / self.period.as_secs_f32())
            .rem_euclid(1.);
        }
        self.sampled_at = now;
        self.was_running = running;
    }
}
impl Render for ActivityIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reduced = cx.reduce_motion();
        let running = self.active && self.visible && !reduced;
        self.advance(Instant::now(), running);
        if !running {
            self.clock = None;
        }
        if running && self.clock.is_none() {
            self.clock = Some(cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let keep_running = view
                        .update(cx, |this, cx| {
                            if !this.active
                                || !this.visible
                                || cx.reduce_motion()
                                || this.painted_at.get().elapsed() > PAINT_LEASE
                            {
                                this.advance(Instant::now(), false);
                                this.clock = None;
                                return false;
                            }
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !keep_running {
                        break;
                    }
                }
            }));
        }
        let (phase, colors, style) = (self.phase, self.colors, self.style);
        let painted_at = self.painted_at.clone();
        div()
            .id("activity-indicator")
            .size(px(self.size))
            .flex_shrink_0()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        if !bounds.intersects(&window.content_mask().bounds) {
                            return;
                        }
                        painted_at.set(Instant::now());
                        let cell = f32::from(bounds.size.width) / 4.;
                        let radius = cell
                            * if style == ActivityStyle::Dots {
                                0.5
                            } else {
                                0.18
                            };
                        for (row, color) in colors.into_iter().enumerate() {
                            for column in 0..3 {
                                let opacity = if reduced {
                                    0.6
                                } else {
                                    brightness(phase, row, column)
                                };
                                let origin = bounds.origin
                                    + point(
                                        px(column as f32 * cell * 1.5),
                                        px(row as f32 * cell * 1.5),
                                    );
                                window.paint_quad(
                                    fill(
                                        Bounds::new(origin, size(px(cell), px(cell))),
                                        color.opacity(opacity),
                                    )
                                    .corner_radii(px(radius)),
                                );
                            }
                        }
                    },
                )
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activity_wave_is_bounded_periodic_symmetric_and_travels_upward() {
        for step in 0..200 {
            let phase = step as f32 / 100.;
            for row in 0..3 {
                for col in 0..3 {
                    let value = brightness(phase, row, col);
                    assert!((0.12..=1.).contains(&value));
                    assert!((value - brightness(phase + 1., row, col)).abs() < 0.00001);
                }
                assert_eq!(brightness(phase, row, 0), brightness(phase, row, 2));
            }
        }
        assert!(brightness(0., 2, 1) > brightness(0., 0, 1));
        assert!(brightness(0.4, 0, 1) > brightness(0.4, 2, 1));
    }
    #[gpui::test]
    fn activity_clock_stops_when_paused_hidden_reduced_or_unpainted(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(false));
        let (view, cx) =
            cx.add_window_view(|w, cx| ActivityIndicator::new(ActivityStyle::Dots, w, cx));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_some()));
        view.update(cx, |v, cx| v.set_active(false, cx));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_none()));
        view.update(cx, |v, cx| v.set_active(true, cx));
        cx.run_until_parked();
        cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
        assert!(view.read_with(cx, |v, _| v.clock.is_none()));
        cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_some()));
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_none()));
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_some()));
        view.read_with(cx, |v, _| {
            v.painted_at.set(Instant::now() - Duration::from_secs(1))
        });
        cx.executor().advance_clock(TICK);
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.clock.is_none()));
    }
}
