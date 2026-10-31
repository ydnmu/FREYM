use crate::ui::{
    primitives::{Stack, Surface, SurfaceVariant, Text, TextVariant},
    theme::colors::Color,
};
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, prelude::*};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastKind {
    #[default]
    Information,
    Warning,
    Error,
}
#[derive(IntoElement)]
pub struct Toast {
    message: SharedString,
    kind: ToastKind,
    action: Option<AnyElement>,
}
impl Toast {
    pub fn new(message: impl Into<SharedString>) -> Self {
        Self {
            message: message.into(),
            kind: ToastKind::Information,
            action: None,
        }
    }
    pub fn kind(mut self, kind: ToastKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }
}
impl RenderOnce for Toast {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let color = match self.kind {
            ToastKind::Information => Color::SecondaryText,
            ToastKind::Warning => Color::Warning,
            ToastKind::Error => Color::Error,
        };
        let mut content = Stack::vertical().child(
            Text::new(self.message)
                .variant(TextVariant::Caption)
                .color(color),
        );
        if let Some(action) = self.action {
            content = content.child(action);
        }
        Surface::new()
            .variant(SurfaceVariant::Elevated)
            .child(content)
    }
}
