use crate::ui::{
    primitives::{Button, ButtonSize, clickable},
    theme::{motion, spacing::Layout},
};
use gpui::{
    AnimationExt, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*,
    px,
};
#[derive(IntoElement)]
pub struct Toggle {
    button: Button,
    checked: bool,
}
impl Toggle {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, checked: bool) -> Self {
        Self {
            button: Button::new(id, label)
                .variant(crate::ui::primitives::ButtonVariant::Ghost)
                .size(ButtonSize::Compact),
            checked,
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub(crate) fn indicator(checked: bool) -> impl IntoElement {
        div()
            .w(px(Layout::TOGGLE_WIDTH))
            .h(px(Layout::TOGGLE_HEIGHT))
            .flex_shrink_0()
            .rounded_full()
            .bg(crate::ui::theme::controls::switch_face(checked, false))
            .child(
                div()
                    .size(px(Layout::TOGGLE_THUMB))
                    .mt(px(Layout::TOGGLE_INSET))
                    .rounded_full()
                    .bg(crate::ui::theme::controls::switch_face(checked, true))
                    .with_spring(
                        "switch-position",
                        motion::selection(if checked { 1. } else { 0. }),
                        |d, t| {
                            d.ml(px(Layout::TOGGLE_INSET
                                + (Layout::TOGGLE_WIDTH
                                    - Layout::TOGGLE_THUMB
                                    - Layout::TOGGLE_INSET * 2.)
                                    * t.clamp(0., 1.)))
                        },
                    ),
            )
    }
}
clickable!(Toggle);
impl RenderOnce for Toggle {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.button
            .toggled(self.checked)
            .content(Self::indicator(self.checked))
    }
}
