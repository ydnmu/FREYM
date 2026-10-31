//! Semantic tokens shared by every native component.
pub mod colors;
pub(crate) mod controls;
pub mod material;
pub mod motion;
pub mod radius;
mod registry;
pub mod spacing;
pub mod surface_treatment;
pub mod typography;
pub use registry::ThemeId;
pub(crate) use registry::set_accent;
pub use surface_treatment::SurfacePreference;
thread_local! {
    static FILLED_BUTTONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LOW_COST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub fn filled_buttons() -> bool {
    FILLED_BUTTONS.get()
}
pub(crate) fn set_filled_buttons(value: bool) {
    FILLED_BUTTONS.set(value);
}
pub fn low_cost_effects() -> bool {
    LOW_COST.get()
}
pub(crate) fn set_low_cost_effects(value: bool) {
    LOW_COST.set(value);
}
use gpui::{BoxShadow, point, px, rgba};

pub struct Theme;
impl Theme {
    pub fn shadow() -> BoxShadow {
        BoxShadow {
            color: rgba(0x363c3012).into(),
            offset: point(px(0.), spacing::Spacing::Xs.px()),
            blur_radius: px(30.),
            spread_radius: -spacing::Spacing::Xxs.px(),
            inset: false,
        }
    }
}
