//! Native surface fills. Window frost is owned by `surface_treatment`.
use super::colors::Color;
use gpui::{Div, div, prelude::*, px};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Material {
    Graphite,
    Smoked,
    Frosted,
}
impl Material {
    pub fn surface(self, low_cost: bool) -> Div {
        self.apply(div(), low_cost)
    }
    pub fn apply(self, surface: Div, low_cost: bool) -> Div {
        let background = if self == Self::Frosted {
            Color::ElevatedSurface.get()
        } else {
            Color::PrimarySurface.get()
        };
        surface
            .relative()
            .bg(if low_cost {
                background.alpha(1.)
            } else {
                background
            })
            .rounded(px(10.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
    }
}

pub fn accent() -> gpui::Hsla {
    Color::Accent.get()
}
pub fn signal() -> gpui::Hsla {
    Color::Record.get()
}
