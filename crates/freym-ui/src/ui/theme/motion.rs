//! Finite transitions; no idle animation loops. GPUI honors reduced motion.
use gpui::{Animation, SpringAnimation, SpringConfig};
use std::time::Duration;
pub const HOVER_MS: u64 = 110;
pub const STATE_MS: u64 = 190;
pub const ENTRANCE_MS: u64 = 240;
pub const EXIT_MS: u64 = 170;
pub const PANEL_TRAVEL: f32 = 24.;
pub const STATE_TRAVEL: f32 = 3.;
pub const SPOTLIGHT_RADIUS: f32 = 160.;
pub const SPOTLIGHT_LAYERS: usize = 20;
pub const METER_SAMPLE_MS: u64 = 40;
pub const METER_ATTACK_SECONDS: f32 = 0.04;
pub const METER_RELEASE_SECONDS: f32 = 0.13;
pub const PRESS_TRAVEL: f32 = 1.5;
pub const PRESS_STIFFNESS: f32 = 1600.;
pub const PRESS_DAMPING: f32 = 80.;
/// Solve Bezier x(t) before y(t); feeding elapsed time directly to y is wrong.
pub fn expansion_ease(progress: f32) -> f32 {
    let x = progress.clamp(0., 1.);
    let (mut low, mut high) = (0., 1.);
    for _ in 0..24 {
        let t = (low + high) / 2.;
        let bx = 3. * (1. - t) * (1. - t) * t * 0.16 + 3. * (1. - t) * t * t * 0.3 + t * t * t;
        if bx < x {
            low = t;
        } else {
            high = t;
        }
    }
    1. - (1. - (low + high) / 2.).powi(3)
}
pub fn expansion(open: bool) -> Animation {
    Animation::new(Duration::from_millis(if open { 200 } else { 145 })).with_easing(expansion_ease)
}
pub fn press(target: f32) -> SpringAnimation<f32> {
    SpringAnimation::new(SpringConfig::new(PRESS_STIFFNESS, PRESS_DAMPING, 1.)).to(target)
}
pub fn entrance() -> Animation {
    transition(ENTRANCE_MS)
}
pub fn state_change() -> Animation {
    transition(STATE_MS)
}
pub fn exit() -> Animation {
    transition(EXIT_MS)
}
pub fn transition(ms: u64) -> Animation {
    Animation::new(Duration::from_millis(ms)).with_easing(|t| 1. - (1. - t).powi(3))
}
pub fn selection(target: f32) -> SpringAnimation<f32> {
    let omega = 5. / (HOVER_MS as f32 / 1000.);
    SpringAnimation::new(SpringConfig::new(omega * omega, 2. * omega, 1.)).to(target)
}
