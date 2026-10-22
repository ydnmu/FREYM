//! Native quad compositing fallback. This is optical glass, not backdrop blur.
use super::{ThemeId, colors::Color};
use gpui::{Div, div, prelude::*, px, rgba};

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
        let background = match (self, low_cost) {
            (Self::Graphite, _) => rgba(0x18191dff),
            (Self::Smoked, false) => rgba(0x202126ed),
            (Self::Frosted, false) => rgba(0x303138f5),
            (Self::Smoked, true) => rgba(0x202126ff),
            (Self::Frosted, true) => rgba(0x303138ff),
        };
        let surface = surface
            .relative()
            .bg(background)
            .rounded(px(10.))
            .border_1()
            .border_color(Color::DefaultBorder.get());
        if low_cost || self == Self::Graphite {
            return surface;
        }
        // A fine top reflection and shorter lower reflection describe a different
        // material than the solid graphite work surface. No textures or idle tasks.
        surface
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(12.))
                    .right(px(12.))
                    .h(px(1.))
                    .bg(rgba(0xffffff30)),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .right(px(18.))
                    .w(px(46.))
                    .h(px(1.))
                    .bg(rgba(0xffffff12)),
            )
    }
}
pub fn accent() -> gpui::Hsla {
    gpui::rgb(if ThemeId::current() == ThemeId::EmberMotion {
        0xca293f
    } else {
        0x7441df
    })
    .into()
}
pub fn signal() -> gpui::Hsla {
    gpui::rgb(if ThemeId::current() == ThemeId::EmberMotion {
        0xff872f
    } else {
        0x438dff
    })
    .into()
}
