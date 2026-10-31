use crate::ui::theme::{colors::Color, spacing::Spacing};
use gpui::{App, IntoElement, RenderOnce, Window, div, prelude::*};
#[derive(IntoElement)]
pub struct Separator;
impl RenderOnce for Separator {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .border_b_1()
            .border_color(Color::SubtleBorder.get())
            .my(Spacing::Xxs.px())
    }
}
