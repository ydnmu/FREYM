use super::{Button, ButtonSize, ButtonVariant, Icons, clickable};
use gpui::{App, ElementId, IntoElement, RenderOnce, Window};
#[derive(IntoElement)]
pub struct IconButton {
    button: Button,
}
impl IconButton {
    pub fn new(
        id: impl Into<ElementId>,
        icon: Icons,
        label: impl Into<gpui::SharedString>,
    ) -> Self {
        Self {
            button: Button::new(id, label)
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Icon)
                .icon(icon),
        }
    }
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.button = self.button.variant(variant);
        self
    }
    pub(crate) fn window_control(mut self, area: gpui::WindowControlArea) -> Self {
        self.button = self.button.window_control(area);
        self
    }
}
clickable!(IconButton);
impl RenderOnce for IconButton {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.button
    }
}
