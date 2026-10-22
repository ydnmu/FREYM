//! Short-lived keyed press state, instantiated exclusively by Motion controls.
use gpui::Context;

#[derive(Default)]
pub(crate) struct MotionPress {
    pub pressed: bool,
}
impl MotionPress {
    pub fn new() -> Self {
        #[cfg(test)]
        LIVE.with(|live| live.set(live.get() + 1));
        Self::default()
    }
    pub fn set(&mut self, pressed: bool, cx: &mut Context<Self>) {
        if self.pressed != pressed {
            self.pressed = pressed;
            cx.notify();
        }
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
