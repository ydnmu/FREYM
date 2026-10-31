//! Short-lived keyed press state, instantiated exclusively by Motion controls.
use crate::ui::theme::motion;
use gpui::{Context, SpringConfig, SpringState};
use std::time::Instant;

pub(crate) struct MotionPress {
    pub pressed: bool,
    pub hovered: bool,
    hover: SpringState,
    press: SpringState,
    hover_target: f32,
    press_target: f32,
    sampled_at: Instant,
    hover_config: SpringConfig,
    press_config: SpringConfig,
}
impl Default for MotionPress {
    fn default() -> Self {
        Self {
            pressed: false,
            hovered: false,
            hover: SpringState {
                position: 0.,
                velocity: 0.,
            },
            press: SpringState {
                position: 0.,
                velocity: 0.,
            },
            hover_target: 0.,
            press_target: 0.,
            sampled_at: Instant::now(),
            hover_config: motion::response_spring(motion::HOVER_MS as f32),
            press_config: motion::response_spring(motion::RELEASE_RESPONSE_MS),
        }
    }
}
impl MotionPress {
    pub fn new() -> Self {
        #[cfg(test)]
        LIVE.with(|live| live.set(live.get() + 1));
        Self::default()
    }

    pub fn set(&mut self, pressed: bool, cx: &mut Context<Self>) {
        if self.pressed != pressed {
            self.retarget_at(pressed, self.hovered, Instant::now());
            cx.notify();
        }
    }
    pub fn hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered != hovered {
            self.retarget_at(self.pressed, hovered, Instant::now());
            cx.notify();
        }
    }
    pub fn nav_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered != hovered {
            self.retarget_nav_at(hovered, Instant::now());
            cx.notify();
        }
    }
    fn retarget_nav_at(&mut self, hovered: bool, now: Instant) {
        self.retarget_at(self.pressed, hovered, now);
        if hovered {
            // Give even a brief navigation hover a visible first frame. Seed the
            // actual spring position so leaving/re-entering preserves that face.
            self.hover.position = self.hover.position.max(0.6);
        }
    }
    fn advance_to(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.sampled_at).as_secs_f32();
        self.hover = self
            .hover_config
            .step(self.hover, self.hover_target, elapsed);
        self.press = self
            .press_config
            .step(self.press, self.press_target, elapsed);
        self.sampled_at = now;
    }
    // Anchor to the input event, so the next draw includes the time since input
    // instead of spending its first display frame stepping toward the old target.
    fn retarget_at(&mut self, pressed: bool, hovered: bool, now: Instant) {
        self.advance_to(now);
        self.pressed = pressed;
        self.hovered = hovered;
        self.press_target = if pressed { 1. } else { 0. };
        self.hover_target = if hovered || pressed { 1. } else { 0. };
    }
    pub fn sample(&mut self, hover: SpringConfig, press: SpringConfig) -> (f32, f32, bool) {
        self.sample_at(hover, press, Instant::now())
    }
    fn sample_at(
        &mut self,
        hover: SpringConfig,
        press: SpringConfig,
        now: Instant,
    ) -> (f32, f32, bool) {
        self.hover_config = hover;
        self.press_config = press;
        self.advance_to(now);
        let hover_done = hover.is_settled(self.hover, self.hover_target, 0.001);
        let press_done = press.is_settled(self.press, self.press_target, 0.001);
        if hover_done {
            self.hover = SpringState {
                position: self.hover_target,
                velocity: 0.,
            };
        }
        if press_done {
            self.press = SpringState {
                position: self.press_target,
                velocity: 0.,
            };
        }
        (
            self.hover.position.clamp(0., 1.),
            self.press.position.clamp(0., 1.),
            !hover_done || !press_done,
        )
    }
}
#[cfg(test)]
thread_local! { static LIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn live_count() -> usize {
    LIVE.get()
}
#[cfg(test)]
impl Drop for MotionPress {
    fn drop(&mut self) {
        LIVE.with(|live| live.set(live.get() - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn press_responds_on_first_frame_and_finishes_without_an_idle_tail() {
        let hover = motion::response_spring(motion::HOVER_MS as f32);
        let down = motion::response_spring(motion::PRESS_RESPONSE_MS);
        let up = motion::response_spring(motion::RELEASE_RESPONSE_MS);
        for hz in [60., 120., 144.] {
            let mut state = MotionPress::new();
            let start = Instant::now();
            state.retarget_at(false, true, start);
            let press_at = start + Duration::from_millis(200);
            assert_eq!(state.sample_at(hover, up, press_at), (1., 0., false));
            state.retarget_at(true, true, press_at);
            let first = press_at + Duration::from_secs_f64(1. / hz);
            assert!(
                state.sample_at(hover, down, first).1 > 0.1,
                "first frame at {hz}"
            );
            let held = press_at + Duration::from_millis(80);
            assert_eq!(state.sample_at(hover, down, held), (1., 1., false));
            state.retarget_at(false, true, held);
            let released = held + Duration::from_millis(150);
            assert_eq!(state.sample_at(hover, up, released), (1., 0., false));
            assert_eq!(
                state.sample_at(hover, up, released + Duration::from_secs(2)),
                (1., 0., false)
            );
        }
    }

    #[test]
    fn reversing_a_press_preserves_position_and_velocity() {
        let mut state = MotionPress::new();
        let hover = motion::response_spring(motion::HOVER_MS as f32);
        let down = motion::response_spring(motion::PRESS_RESPONSE_MS);
        let up = motion::response_spring(motion::RELEASE_RESPONSE_MS);
        let start = Instant::now();
        state.retarget_at(true, true, start);
        let midway = start + Duration::from_millis(20);
        state.sample_at(hover, down, midway);
        let before = state.press;
        state.retarget_at(false, true, midway);
        assert!((state.press.position - before.position).abs() < 0.00001);
        assert!((state.press.velocity - before.velocity).abs() < 0.00001);
        for ms in [27, 35, 50, 80, 120, 180] {
            let (_, press, _) = state.sample_at(hover, up, start + Duration::from_millis(ms));
            assert!((0. ..=1.).contains(&press));
        }
        assert_eq!(
            state.sample_at(hover, up, start + Duration::from_millis(250)),
            (1., 0., false)
        );
    }
    #[test]
    fn rapid_navigation_hovers_paint_immediately_reverse_and_settle() {
        let mut rows = (0..8).map(|_| MotionPress::new()).collect::<Vec<_>>();
        let start = Instant::now();
        let hover = motion::response_spring(motion::HOVER_MS as f32);
        let press = motion::response_spring(motion::RELEASE_RESPONSE_MS);
        for (index, state) in rows.iter_mut().enumerate() {
            let entered = start + Duration::from_millis(index as u64 * 3);
            state.retarget_nav_at(true, entered);
            assert!(state.sample_at(hover, press, entered).0 >= 0.6);
            let left = entered + Duration::from_millis(2);
            let position = state.sample_at(hover, press, left).0;
            state.retarget_nav_at(false, left);
            assert!((state.sample_at(hover, press, left).0 - position).abs() < 0.00001);
            assert!(
                state
                    .sample_at(hover, press, left + Duration::from_millis(1))
                    .0
                    > 0.5
            );
            assert_eq!(
                state.sample_at(hover, press, start + Duration::from_secs(2)),
                (0., 0., false)
            );
        }
    }
}
