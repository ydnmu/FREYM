use crate::ui::primitives::{Button, ButtonSize, Icons, clickable};
use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString, Window};
#[derive(IntoElement)]
pub struct DeviceSelector {
    button: Button,
}
impl DeviceSelector {
    pub fn new(id: impl Into<ElementId>, device: impl Into<SharedString>) -> Self {
        Self {
            button: Button::new(id, device)
                .size(ButtonSize::Row)
                .icon(Icons::Camera),
        }
    }
}
clickable!(DeviceSelector);
impl RenderOnce for DeviceSelector {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.button
    }
}
