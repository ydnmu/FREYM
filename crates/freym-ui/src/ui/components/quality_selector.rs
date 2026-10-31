use super::SegmentedControl;
use crate::ui::{
    primitives::{Button, ButtonSize, Stack, Text, TextVariant, clickable},
    theme::spacing::Spacing,
};
use gpui::{AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, prelude::*};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityMode {
    Automatic,
    Custom,
}
#[derive(IntoElement)]
pub struct QualitySelector {
    button: Button,
}
impl QualitySelector {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            button: Button::new(id, label).size(ButtonSize::Compact).segment(),
        }
    }
}
clickable!(QualitySelector);
impl RenderOnce for QualitySelector {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.button
    }
}
#[derive(IntoElement)]
pub struct RecordingQualityPanel {
    automatic: AnyElement,
    custom: AnyElement,
    details: SharedString,
    selected: usize,
}
impl RecordingQualityPanel {
    pub fn new(
        mode: QualityMode,
        automatic: QualitySelector,
        custom: QualitySelector,
        details: impl Into<SharedString>,
    ) -> Self {
        Self {
            automatic: automatic
                .selected(mode == QualityMode::Automatic)
                .into_any_element(),
            custom: custom
                .selected(mode == QualityMode::Custom)
                .into_any_element(),
            details: details.into(),
            selected: if mode == QualityMode::Automatic { 0 } else { 1 },
        }
    }
}
impl RenderOnce for RecordingQualityPanel {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Stack::vertical()
            .gap(Spacing::Xxs)
            .align(crate::ui::primitives::Alignment::Center)
            .child(
                SegmentedControl::new()
                    .id("quality-mode-switch")
                    .selected(self.selected)
                    .child(self.automatic)
                    .child(self.custom),
            )
            .child(Text::new(self.details).variant(TextVariant::Muted))
    }
}
