use crate::ui::{
    primitives::{Stack, Text, TextVariant},
    theme::spacing::Spacing,
};
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};
#[derive(IntoElement)]
pub struct SettingRow {
    label: SharedString,
    description: Option<SharedString>,
    control: AnyElement,
}
impl SettingRow {
    pub fn new(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self {
            label: label.into(),
            description: None,
            control: control.into_any_element(),
        }
    }
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }
}
impl RenderOnce for SettingRow {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let settings = crate::ui::primitives::control_scope::current().settings;
        let debug = format!("setting-row-{}", self.label);
        let mut label = Stack::vertical()
            .gap(Spacing::Xxs)
            .child(Text::new(self.label).variant(TextVariant::Label));
        if let Some(description) = self.description {
            label = label.child(Text::new(description).variant(TextVariant::Muted));
        }
        div()
            .debug_selector(move || debug.clone())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_between()
            .gap(Spacing::Md.px())
            .when(settings, |d| {
                d.min_h(gpui::px(42.))
                    .py(gpui::px(5.))
                    .border_b_1()
                    .border_color(
                        crate::ui::theme::colors::Color::SubtleBorder
                            .get()
                            .opacity(0.5),
                    )
            })
            .when(
                !settings && crate::ui::theme::ThemeId::current().is_motion(),
                |d| {
                    d.py(gpui::px(10.))
                        .border_b_1()
                        .border_color(crate::ui::theme::colors::Color::SubtleBorder.get())
                },
            )
            .child(div().flex_1().min_w_0().child(label))
            .child(
                div()
                    .min_w_0()
                    .when(settings, |d| {
                        d.w(gpui::px(
                            if f32::from(window.viewport_size().width) < 1000. {
                                200.
                            } else {
                                320.
                            },
                        ))
                        .max_w(gpui::relative(0.6))
                        .flex()
                        .justify_end()
                    })
                    .when(!settings, |d| d.flex_shrink_0())
                    .child(self.control),
            )
    }
}
