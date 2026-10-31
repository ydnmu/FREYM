use crate::ui::theme::spacing::Spacing;
use gpui::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Window, div, prelude::*};
#[derive(Clone, Copy, Default)]
pub enum Alignment {
    #[default]
    Start,
    Center,
    Between,
    End,
}
#[derive(IntoElement)]
pub struct Stack {
    vertical: bool,
    gap: Spacing,
    alignment: Alignment,
    children: Vec<AnyElement>,
}
impl Stack {
    pub fn vertical() -> Self {
        Self {
            vertical: true,
            gap: Spacing::Sm,
            alignment: Alignment::Start,
            children: vec![],
        }
    }
    pub fn horizontal() -> Self {
        Self {
            vertical: false,
            ..Self::vertical()
        }
    }
    pub fn gap(mut self, gap: Spacing) -> Self {
        self.gap = gap;
        self
    }
    pub fn align(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }
}
impl ParentElement for Stack {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for Stack {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .min_w_0()
            .flex()
            .gap(self.gap.px())
            .when(self.vertical, |d| d.w_full().flex_col().flex_shrink_0())
            .when(!self.vertical, |d| d.items_center().flex_wrap())
            .map(|d| match self.alignment {
                Alignment::Start => d,
                Alignment::Center => d.items_center().justify_center(),
                Alignment::Between => d.justify_between(),
                Alignment::End => d.justify_end(),
            })
            .children(self.children)
    }
}
