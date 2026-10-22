use crate::ui::{
    primitives::{Button, ButtonSize, Icon, Icons, Text, TextVariant, clickable},
    theme::spacing::Spacing,
};
use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};
#[derive(IntoElement)]
pub struct OutputFolderRow {
    button: Button,
    detail: Option<SharedString>,
    folder: SharedString,
}
impl OutputFolderRow {
    pub fn new(id: impl Into<ElementId>, folder: impl Into<SharedString>) -> Self {
        let folder = folder.into();
        Self {
            button: Button::new(id, folder.clone()).size(ButtonSize::Compact),
            detail: None,
            folder,
        }
    }
    pub fn information(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}
clickable!(OutputFolderRow);
impl RenderOnce for OutputFolderRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let mut row = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(Spacing::Xxs.px())
            .child(
                self.button.content(
                    div()
                        .min_w_0()
                        .max_w(gpui::px(420.))
                        .flex()
                        .items_center()
                        .gap(Spacing::Xs.px())
                        .child(Icon::new(Icons::Folder))
                        .child(
                            Text::new(self.folder)
                                .variant(TextVariant::Caption)
                                .truncate(),
                        ),
                ),
            );
        if let Some(detail) = self.detail {
            row = row.child(Text::new(detail).variant(TextVariant::Muted).truncate());
        }
        row
    }
}
